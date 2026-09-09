#include "monitor_model.h"
#include <QFileInfo>
#include <QJsonArray>
#include <algorithm>
#include <utility>

QString DesktopView::controllerText() const {
    if (!infoCurrent)
        return "Controller state unknown · information outdated";
    if (!available)
        return "Desktop unavailable · controller state unknown";
    return claimed ? "Controller connected" : "No controller";
}
MonitorModel::MonitorModel(QString socketPath, QObject *parent)
    : QObject(parent), client_(std::move(socketPath), this) {
    clock_.start();
    informationTimer_.setInterval(5000);
    previewTimer_.setInterval(100);
    connect(&informationTimer_, &QTimer::timeout, this, &MonitorModel::refresh);
    connect(&previewTimer_, &QTimer::timeout, this, &MonitorModel::previews);
    connect(&client_, &DaemonClient::connectionChanged, this,
            [this](bool ready, const QString &message) {
                if (closed_)
                    return;
                message_ = message;
                if (ready)
                    refresh();
                else {
                    listRequest_ = 0;
                    for (auto &d : desktops_) {
                        d.infoCurrent = false;
                        d.previewRequest = 0;
                    }
                }
                emit changed();
            });
}
MonitorModel::~MonitorModel() { close(); }
void MonitorModel::start() {
    informationTimer_.start();
    previewTimer_.start();
    reconnect();
}
void MonitorModel::reconnect() {
    if (!closed_)
        client_.connectToDaemon();
}
void MonitorModel::refresh() {
    if (closed_ || !client_.ready() || listRequest_)
        return;
    listRequest_ = client_.list([this](QJsonObject reply, QByteArray, QString error) {
        listRequest_ = 0;
        if (error.isEmpty() && (reply["kind"] != "desktops" || !reply["result"].isArray()))
            error = "Invalid desktop listing";
        if (!error.isEmpty()) {
            for (auto &d : desktops_)
                d.infoCurrent = false;
            message_ = "Desktop information is outdated: " + error;
            emit changed();
            return;
        }
        QSet<QString> present;
        for (const auto &value : reply["result"].toArray()) {
            const auto info = value.toObject();
            const auto descriptor = info["descriptor"].toObject();
            const auto id = descriptor["id"].toString();
            if (!descriptor["owned"].toBool() || id.isEmpty() || id == "main")
                continue;
            present.insert(id);
            auto &d = desktops_[id];
            d.id = id;
            d.name = descriptor["name"].toString();
            d.available = info["available"].toBool();
            d.claimed = info["claimed"].toBool();
            d.infoCurrent = true;
            d.controlError = info["control_error"].toString();
            QStringList apps;
            for (auto app : info["applications"].toArray()) {
                const auto record = app.toObject();
                QString label = QFileInfo(record["executable"].toString()).fileName();
                if (label.isEmpty())
                    continue;
                if (!record["running"].toBool())
                    label += " (launcher exited)";
                apps.append(label);
            }
            apps.removeDuplicates();
            d.launches = apps.isEmpty() ? "No tracked launches" : apps.join(", ");
            if (!d.available && d.previewRequest) {
                client_.cancel(d.previewRequest);
                d.previewRequest = 0;
            }
        }
        for (auto it = desktops_.begin(); it != desktops_.end();) {
            if (!present.contains(it.key())) {
                client_.cancel(it->previewRequest);
                it = desktops_.erase(it);
            } else
                ++it;
        }
        if (!selected_.isEmpty() && !desktops_.contains(selected_))
            selected_.clear();
        message_ = "Local machine · " + QString::number(desktops_.size()) + " owned desktops";
        emit changed();
        previews();
    });
}
void MonitorModel::cancelPreviews() {
    for (auto &d : desktops_) {
        if (d.previewRequest)
            client_.cancel(d.previewRequest);
        d.previewRequest = 0;
        d.nextPreview = 0;
    }
}
void MonitorModel::select(const QString &id) {
    if (!id.isEmpty() && !desktops_.contains(id))
        return;
    cancelPreviews();
    selected_ = id;
    emit changed();
    previews();
}
void MonitorModel::setPreviewEnabled(bool enabled) {
    if (previewEnabled_ == enabled)
        return;
    previewEnabled_ = enabled;
    if (!enabled)
        cancelPreviews();
    else
        previews();
}
void MonitorModel::previews() {
    if (closed_ || !client_.ready() || !previewEnabled_)
        return;
    int pending = 0;
    for (const auto &d : desktops_)
        pending += d.previewRequest != 0;
    QStringList eligible;
    for (const auto &d : desktops_) {
        if (!d.infoCurrent || !d.available || d.previewRequest || destroying_.contains(d.id) ||
            (!selected_.isEmpty() && selected_ != d.id) || d.nextPreview > now())
            continue;
        eligible.append(d.id);
    }
    // Slow requests must not reclaim both slots ahead of desktops still waiting.
    std::stable_sort(eligible.begin(), eligible.end(), [this](const auto &a, const auto &b) {
        return desktops_[a].nextPreview < desktops_[b].nextPreview;
    });
    for (const auto &id : eligible) {
        if (pending >= 2)
            break;
        auto &d = desktops_[id];
        d.nextPreview = now() + (selected_.isEmpty() ? 2000 : 500);
        d.previewRequest = client_.observe(
            id, selected_.isEmpty() ? 640 : 1600,
            [this, id](QJsonObject reply, QByteArray png, QString error) {
                if (!desktops_.contains(id))
                    return;
                auto &d = desktops_[id];
                d.previewRequest = 0;
                const auto observation = reply["result"].toObject();
                if (error.isEmpty() &&
                    (reply["kind"] != "control" || observation["kind"] != "observation"))
                    error = "Invalid observation reply";
                QImage image;
                if (error.isEmpty() && !image.loadFromData(png, "PNG"))
                    error = "Invalid preview image";
                d.previewError = error;
                if (error.isEmpty()) {
                    d.image = image;
                    d.receivedAt = now();
                    const auto info = observation["result"].toObject()["info"].toObject();
                    d.captureAge = qMax<qint64>(0, info["age_ms"].toDouble());
                }
                emit changed();
            });
        ++pending;
    }
}
void MonitorModel::destroyConfirmed(const QString &id) {
    if (closed_ || !client_.ready() || id == "main" || !desktops_.contains(id) ||
        destroying_.contains(id))
        return;
    destroying_.insert(id);
    auto &d = desktops_[id];
    client_.cancel(d.previewRequest);
    d.previewRequest = 0;
    emit changed();
    client_.destroy(id, [this, id](QJsonObject reply, QByteArray, QString error) {
        destroying_.remove(id);
        const bool confirmed = error.isEmpty() && reply["kind"] == "destroyed" &&
                               reply["result"].toObject()["desktop_id"] == id;
        QString message;
        if (confirmed) {
            desktops_.remove(id);
            if (selected_ == id)
                selected_.clear();
            message = "Desktop destruction confirmed.";
        } else {
            message = "Destruction unconfirmed. It may still be finishing; it was not retried. " +
                      (error.isEmpty() ? QString("Unexpected reply") : error);
        }
        // Cancel a pre-destruction list so an old snapshot cannot resurrect a tile.
        client_.cancel(listRequest_);
        listRequest_ = 0;
        emit changed();
        emit destructionFinished(id, confirmed, message);
        refresh();
    });
}
void MonitorModel::close() {
    if (closed_)
        return;
    closed_ = true;
    informationTimer_.stop();
    previewTimer_.stop();
    client_.close();
}
