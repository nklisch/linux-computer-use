#pragma once
#include <QElapsedTimer>
#include <QHash>
#include <QJsonObject>
#include <QLocalSocket>
#include <QObject>
#include <QTimer>
#include <functional>

// The only commands exposed to the view are read-only observation and explicit
// destruction. This connection must never claim, authorize or send desktop input.
class DaemonClient : public QObject {
    Q_OBJECT
public:
    using Completion = std::function<void(QJsonObject, QByteArray, QString)>;
    explicit DaemonClient(QString socketPath, QObject *parent = nullptr);
    void connectToDaemon();
    bool ready() const { return ready_; }
    quint64 list(Completion done);
    quint64 observe(const QString &desktop, int maxDimension, Completion done);
    quint64 destroy(const QString &desktop, Completion done);
    void cancel(quint64 id);
    void close();
    int pendingCount() const { return pending_.size(); }
signals:
    void connectionChanged(bool ready, const QString &message);

private:
    struct Pending {
        Completion done;
        qint64 deadline;
    };
    quint64 call(QJsonObject request, int timeoutMs, Completion done);
    void send(const QJsonObject &packet);
    void receive();
    void lost(const QString &message);
    QLocalSocket socket_;
    QString path_;
    QByteArray buffer_;
    QHash<quint64, Pending> pending_;
    QElapsedTimer clock_;
    QTimer deadlines_;
    quint64 next_ = 1;
    bool ready_ = false;
    bool closing_ = false;
};
