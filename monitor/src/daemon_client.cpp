#include "daemon_client.h"
#include <QJsonDocument>
#include <utility>

DaemonClient::DaemonClient(QString socketPath, QObject *parent)
    : QObject(parent), path_(std::move(socketPath)) {
    clock_.start();
    deadlines_.setInterval(100);
    connect(&deadlines_, &QTimer::timeout, this, [this] {
        const auto ids = pending_.keys();
        for (auto id : ids) {
            if (!pending_.contains(id) || pending_[id].deadline > clock_.elapsed())
                continue;
            auto done = std::move(pending_[id].done);
            cancel(id);
            done({}, {},
                 "Response timed out. The operation may still be finishing; no automatic retry was "
                 "sent.");
        }
    });
    deadlines_.start();
    connect(&socket_, &QLocalSocket::connected, this, [this] {
        call({{"request", "hello"}}, 3000, [this](QJsonObject reply, QByteArray, QString error) {
            if (!error.isEmpty() || reply["kind"] != "hello" ||
                reply["result"].toObject()["wire_revision"].toInt(-1) != LCU_WIRE_REVISION) {
                lost(error.isEmpty() ? "Incompatible LCU protocol. Use matching binaries; no "
                                       "services were restarted."
                                     : error);
                return;
            }
            ready_ = true;
            emit connectionChanged(true, "Connected to local LCU");
        });
    });
    connect(&socket_, &QLocalSocket::readyRead, this, &DaemonClient::receive);
    connect(&socket_, &QLocalSocket::disconnected, this, [this] {
        if (!closing_)
            lost("Connection lost. Desktop state is unknown; desktops were not stopped.");
    });
    connect(&socket_, &QLocalSocket::errorOccurred, this, [this](QLocalSocket::LocalSocketError) {
        if (!closing_)
            lost("Cannot connect to LCU: " + socket_.errorString() + ". No daemon was started.");
    });
}
void DaemonClient::connectToDaemon() {
    if (socket_.state() != QLocalSocket::UnconnectedState)
        return;
    closing_ = false;
    buffer_.clear();
    socket_.connectToServer(path_);
}
void DaemonClient::send(const QJsonObject &packet) {
    socket_.write(QJsonDocument(packet).toJson(QJsonDocument::Compact) + '\n');
}
quint64 DaemonClient::call(QJsonObject request, int timeoutMs, Completion done) {
    const auto id = next_++;
    if (socket_.state() != QLocalSocket::ConnectedState) {
        QTimer::singleShot(0, this,
                           [done = std::move(done)] { done({}, {}, "Not connected to LCU"); });
        return id;
    }
    pending_.insert(id, {std::move(done), clock_.elapsed() + timeoutMs});
    send({{"type", "call"}, {"id", double(id)}, {"request", request}});
    return id;
}
quint64 DaemonClient::list(Completion done) {
    return call({{"request", "desktop"}, {"operation", "list"}}, 5000, std::move(done));
}
quint64 DaemonClient::observe(const QString &desktop, int maxDimension, Completion done) {
    return call({{"request", "control"},
                 {"desktop_id", desktop},
                 {"command", QJsonObject{{"command", "observe"},
                                         {"args", QJsonObject{{"max_dimension", maxDimension},
                                                              {"timeout_ms", 0}}}}}},
                3000, std::move(done));
}
quint64 DaemonClient::destroy(const QString &desktop, Completion done) {
    return call({{"request", "desktop"},
                 {"operation", "destroy"},
                 {"desktop_id", desktop},
                 {"force", true}},
                30000, std::move(done));
}
void DaemonClient::cancel(quint64 id) {
    if (pending_.remove(id) && socket_.state() == QLocalSocket::ConnectedState)
        send({{"type", "cancel"}, {"id", double(id)}});
}
void DaemonClient::receive() {
    buffer_ += socket_.readAll();
    // A bounded monitor image is at most 1600px on its longest edge. A malformed
    // endpoint must not grow the UI's line buffer without bound.
    if (buffer_.size() > 32 * 1024 * 1024) {
        lost("LCU reply exceeded the monitor's image budget");
        return;
    }
    while (true) {
        const auto end = buffer_.indexOf('\n');
        if (end < 0)
            return;
        const auto line = buffer_.left(end);
        buffer_.remove(0, end + 1);
        QJsonParseError error;
        const auto doc = QJsonDocument::fromJson(line, &error);
        const auto packet = doc.object();
        if (error.error != QJsonParseError::NoError || packet["type"] != "reply") {
            lost("Invalid LCU reply; connection closed without sending input");
            return;
        }
        const auto id = quint64(packet["id"].toDouble());
        if (!pending_.contains(id))
            continue; // Obsolete/cancelled response.
        const auto completion = pending_.take(id).done;
        const auto result = packet["result"].toObject();
        if (result.contains("Err"))
            completion({}, {}, result["Err"].toString("LCU request failed"));
        else if (result["Ok"].isObject()) {
            completion(result["Ok"].toObject(),
                       QByteArray::fromBase64(packet["image"].toString().toLatin1()), {});
        } else
            completion({}, {}, "Invalid LCU result");
    }
}
void DaemonClient::lost(const QString &message) {
    if (closing_)
        return;
    closing_ = true;
    ready_ = false;
    socket_.abort();
    buffer_.clear();
    const auto pending = std::exchange(pending_, {});
    for (const auto &p : pending)
        p.done({}, {}, message);
    emit connectionChanged(false, message);
    closing_ = false;
}
void DaemonClient::close() {
    closing_ = true;
    ready_ = false;
    pending_.clear();
    socket_.abort(); // Disconnect only. Never stop/release/destroy a desktop.
}
