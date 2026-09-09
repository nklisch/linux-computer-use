#pragma once
#include "monitor_model.h"
#include "preview_widget.h"
#include <QFrame>
#include <QGridLayout>
#include <QLabel>
#include <QMainWindow>
#include <QPushButton>
#include <QScrollArea>
#include <QStackedWidget>
#include <QToolButton>

class DesktopTile : public QFrame {
    Q_OBJECT
public:
    DesktopTile(QString id, QWidget *parent = nullptr);
    void updateView(const DesktopView &view, bool destroying, bool connected, qint64 now);
    PreviewWidget *preview;
signals:
    void openRequested(const QString &id);
    void destroyRequested(const QString &id);

private:
    QString id_;
    QLabel *title_, *owner_, *health_, *apps_;
    QToolButton *menu_;
};
class MonitorWindow : public QMainWindow {
    Q_OBJECT
public:
    explicit MonitorWindow(MonitorModel *model, QWidget *parent = nullptr);

protected:
    void resizeEvent(QResizeEvent *) override;
    void changeEvent(QEvent *) override;
    void closeEvent(QCloseEvent *) override;

private:
    void updateView();
    void arrange();
    void confirmDestroy(const QString &id);
    MonitorModel *model_;
    QMap<QString, DesktopTile *> tiles_;
    QGridLayout *grid_;
    QScrollArea *scroll_;
    QStackedWidget *stack_;
    PreviewWidget *expanded_;
    QLabel *title_, *subtitle_, *connection_, *empty_, *notice_, *viewerInfo_;
    QPushButton *back_, *reconnect_;
    int columns_ = 0;
};
