#pragma once
#include "daemon_client.h"
#include <QImage>
#include <QMap>
#include <QSet>

struct DesktopView {
    QString id, name, launches, controlError, previewError;
    bool available = false, claimed = false, infoCurrent = false;
    QImage image;
    qint64 receivedAt = -1, captureAge = 0, nextPreview = 0;
    quint64 previewRequest = 0;
    QString title() const { return name.isEmpty() ? id : name; }
    QString controllerText() const;
};

// Transient presentation, not another desktop registry. Successful listing is
// authoritative; failed listing keeps the last view explicitly marked outdated.
class MonitorModel : public QObject {
    Q_OBJECT
public:
    explicit MonitorModel(QString socketPath, QObject *parent = nullptr);
    ~MonitorModel() override;
    const QMap<QString, DesktopView> &desktops() const { return desktops_; }
    QString selected() const { return selected_; }
    QString message() const { return message_; }
    bool connected() const { return client_.ready(); }
    bool destroying(const QString &id) const { return destroying_.contains(id); }
    qint64 now() const { return clock_.elapsed(); }
    void start();
    void reconnect();
    void refresh();
    void select(const QString &id);
    void setPreviewEnabled(bool enabled);
    // Caller must obtain explicit human confirmation for this exact ID.
    void destroyConfirmed(const QString &id);
    void close();
signals:
    void changed();
    void destructionFinished(const QString &id, bool confirmed, const QString &message);

private:
    void previews();
    void cancelPreviews();
    DaemonClient client_;
    QTimer informationTimer_, previewTimer_;
    QElapsedTimer clock_;
    QMap<QString, DesktopView> desktops_;
    QSet<QString> destroying_;
    QString selected_, message_ = "Connecting to local LCU…";
    quint64 listRequest_ = 0;
    bool previewEnabled_ = true, closed_ = false;
};
