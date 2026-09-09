#include "monitor_window.h"
#include <QCloseEvent>
#include <QFileInfo>
#include <QHBoxLayout>
#include <QMenu>
#include <QMessageBox>
#include <QShortcut>
#include <QStatusBar>
#include <QVBoxLayout>

namespace {
QLabel *label(QWidget *parent, bool muted = false) {
    auto *l = new QLabel(parent);
    l->setTextFormat(Qt::PlainText);
    l->setWordWrap(true);
    if (muted) {
        auto font = l->font();
        font.setPointSizeF(qMax(9., font.pointSizeF() - 1));
        l->setFont(font);
    }
    return l;
}
QString previewOverlay(const DesktopView &d) {
    if (!d.infoCurrent)
        return "Information outdated\nLast image retained";
    if (!d.available)
        return "Desktop unavailable\nLast image retained";
    if (!d.previewError.isEmpty())
        return "Preview unavailable\nLast image retained";
    return d.image.isNull() ? "Waiting for preview…" : QString();
}
QString ageText(const DesktopView &d, qint64 now) {
    if (d.receivedAt < 0)
        return "No preview received";
    const auto elapsed = qMax<qint64>(0, now - d.receivedAt);
    return QString("Received %1s ago · image age %2s")
        .arg(elapsed / 1000)
        .arg((d.captureAge + elapsed) / 1000);
}
} // namespace
DesktopTile::DesktopTile(QString id, QWidget *parent) : QFrame(parent), id_(std::move(id)) {
    setFrameShape(QFrame::StyledPanel);
    setSizePolicy(QSizePolicy::Expanding, QSizePolicy::Fixed);
    auto *layout = new QVBoxLayout(this);
    layout->setContentsMargins(0, 0, 0, 0);
    layout->setSpacing(0);
    preview = new PreviewWidget(this);
    preview->setObjectName("preview-" + id_);
    layout->addWidget(preview, 1);
    auto *details = new QWidget(this);
    auto *row = new QHBoxLayout(details);
    auto *names = new QVBoxLayout;
    title_ = label(details);
    auto font = title_->font();
    font.setBold(true);
    title_->setFont(font);
    owner_ = label(details, true);
    names->addWidget(title_);
    names->addWidget(owner_);
    row->addLayout(names, 1);
    menu_ = new QToolButton(details);
    menu_->setText("⋮");
    menu_->setMinimumSize(36, 36);
    menu_->setObjectName("options-" + id_);
    menu_->setPopupMode(QToolButton::InstantPopup);
    auto *menu = new QMenu(menu_);
    menu->addAction("Open view", this, [this] { emit openRequested(id_); });
    menu->addSeparator();
    menu->addAction("Destroy desktop…", this, [this] { emit destroyRequested(id_); });
    menu_->setMenu(menu);
    row->addWidget(menu_);
    layout->addWidget(details);
    auto *footer = new QWidget(this);
    auto *bottom = new QVBoxLayout(footer);
    bottom->setContentsMargins(10, 0, 10, 10);
    bottom->setSpacing(3);
    health_ = label(footer, true);
    apps_ = label(footer, true);
    bottom->addWidget(health_);
    bottom->addWidget(apps_);
    layout->addWidget(footer);
    for (auto *text : {title_, owner_, health_, apps_})
        text->setSizePolicy(QSizePolicy::Ignored, QSizePolicy::Preferred);
    connect(preview, &QAbstractButton::clicked, this, [this] { emit openRequested(id_); });
}
void DesktopTile::updateView(const DesktopView &d, bool destroying, bool connected, qint64 now) {
    title_->setText(d.title());
    title_->setToolTip(d.id);
    owner_->setText(d.controllerText());
    preview->setAccessibleName("View " + d.title());
    menu_->setAccessibleName("Options for " + d.title());
    preview->setPreview(d.image, destroying ? "Destroying desktop…" : previewOverlay(d));
    health_->setText(destroying ? "Destruction pending" : ageText(d, now));
    health_->setToolTip(d.previewError.isEmpty() ? d.controlError : d.previewError);
    apps_->setText("Launches: " + d.launches);
    apps_->setToolTip("Tracked executable launches, not the active window. A launcher can exit "
                      "while its windows remain.");
    menu_->setEnabled(!destroying && connected);
    preview->setEnabled(!destroying);
}
MonitorWindow::MonitorWindow(MonitorModel *model, QWidget *parent)
    : QMainWindow(parent), model_(model) {
    setWindowTitle("Agent Desktops");
    setWindowIcon(QIcon::fromTheme("preferences-desktop-display"));
    resize(1200, 900);
    setMinimumSize(520, 420);
    auto *body = new QWidget(this);
    auto *layout = new QVBoxLayout(body);
    layout->setContentsMargins(16, 12, 16, 8);
    layout->setSpacing(12);
    auto *toolbar = new QHBoxLayout;
    back_ = new QPushButton(QIcon::fromTheme("go-previous"), "Back", body);
    back_->setObjectName("back");
    back_->setAccessibleName("Back to all desktops");
    toolbar->addWidget(back_);
    auto *heading = new QVBoxLayout;
    title_ = label(body);
    subtitle_ = label(body, true);
    auto font = title_->font();
    font.setPointSizeF(font.pointSizeF() + 4);
    font.setBold(true);
    title_->setFont(font);
    heading->addWidget(title_);
    heading->addWidget(subtitle_);
    toolbar->addLayout(heading, 1);
    auto *badge = label(body);
    badge->setText("View only");
    toolbar->addWidget(badge);
    reconnect_ = new QPushButton("Reconnect", body);
    reconnect_->setObjectName("reconnect");
    toolbar->addWidget(reconnect_);
    layout->addLayout(toolbar);
    connection_ = label(body, true);
    layout->addWidget(connection_);
    notice_ = label(body);
    notice_->setObjectName("notice");
    notice_->hide();
    layout->addWidget(notice_);
    stack_ = new QStackedWidget(body);
    layout->addWidget(stack_, 1);
    scroll_ = new QScrollArea(stack_);
    scroll_->setWidgetResizable(true);
    scroll_->setFrameShape(QFrame::NoFrame);
    auto *gridHost = new QWidget(scroll_);
    grid_ = new QGridLayout(gridHost);
    grid_->setContentsMargins(0, 0, 0, 0);
    grid_->setSpacing(14);
    grid_->setAlignment(Qt::AlignTop);
    scroll_->setWidget(gridHost);
    stack_->addWidget(scroll_);
    auto *viewer = new QWidget(stack_);
    auto *viewerLayout = new QVBoxLayout(viewer);
    viewerLayout->setContentsMargins(0, 0, 0, 0);
    expanded_ = new PreviewWidget(viewer);
    expanded_->setObjectName("expanded-preview");
    expanded_->setAccessibleName("Read-only desktop image");
    expanded_->setFocusPolicy(Qt::NoFocus);
    viewerLayout->addWidget(expanded_, 1);
    viewerInfo_ = label(viewer, true);
    viewerLayout->addWidget(viewerInfo_);
    stack_->addWidget(viewer);
    empty_ = label(stack_);
    empty_->setAlignment(Qt::AlignCenter);
    stack_->addWidget(empty_);
    auto *footer = label(body, true);
    footer->setText("Closing this window leaves desktops and applications running.");
    layout->addWidget(footer);
    setCentralWidget(body);
    connect(model_, &MonitorModel::changed, this, &MonitorWindow::updateView);
    connect(model_, &MonitorModel::destructionFinished, this,
            [this](const QString &, bool, const QString &message) {
                notice_->setText(message);
                notice_->show();
            });
    connect(back_, &QPushButton::clicked, this, [this] { model_->select({}); });
    connect(reconnect_, &QPushButton::clicked, model_, &MonitorModel::reconnect);
    auto *escape = new QShortcut(QKeySequence(Qt::Key_Escape), this);
    connect(escape, &QShortcut::activated, this, [this] { model_->select({}); });
    auto *ageTimer = new QTimer(this);
    connect(ageTimer, &QTimer::timeout, this, &MonitorWindow::updateView);
    ageTimer->start(1000); // Retained evidence keeps aging even without network replies.
    updateView();
}
void MonitorWindow::updateView() {
    bool topology = false;
    for (auto it = tiles_.begin(); it != tiles_.end();) {
        if (!model_->desktops().contains(it.key())) {
            delete it.value();
            it = tiles_.erase(it);
            topology = true;
        } else
            ++it;
    }
    for (auto it = model_->desktops().cbegin(); it != model_->desktops().cend(); ++it) {
        if (!tiles_.contains(it.key())) {
            auto *tile = new DesktopTile(it.key());
            tiles_.insert(it.key(), tile);
            topology = true;
            connect(tile, &DesktopTile::openRequested, model_, &MonitorModel::select);
            connect(tile, &DesktopTile::destroyRequested, this, &MonitorWindow::confirmDestroy);
        }
        tiles_[it.key()]->updateView(it.value(), model_->destroying(it.key()), model_->connected(),
                                     model_->now());
    }
    if (topology)
        columns_ = 0;
    // The stacked page's viewport may still have its hidden initial geometry.
    QTimer::singleShot(0, this, &MonitorWindow::arrange);
    connection_->setText(model_->message());
    reconnect_->setVisible(!model_->connected());
    back_->setVisible(!model_->selected().isEmpty());
    if (!model_->selected().isEmpty() && model_->desktops().contains(model_->selected())) {
        const auto &d = model_->desktops()[model_->selected()];
        title_->setText(d.title());
        subtitle_->setText(d.controllerText() + " · " + d.id);
        expanded_->setPreview(d.image, previewOverlay(d));
        viewerInfo_->setText(ageText(d, model_->now()) + " · Viewing never sends input");
        viewerInfo_->setToolTip(d.previewError.isEmpty() ? d.controlError : d.previewError);
        stack_->setCurrentIndex(1);
    } else {
        title_->setText("Desktops");
        subtitle_->setText("Preview overview · refreshes about every 2 seconds");
        if (tiles_.isEmpty()) {
            empty_->setText(
                model_->connected()
                    ? "No agent desktops\nDesktops created by agents will appear here."
                    : "LCU is not connected\nUse Reconnect when the existing daemon is available.");
            stack_->setCurrentIndex(2);
        } else
            stack_->setCurrentIndex(0);
    }
}
void MonitorWindow::arrange() {
    const int width = this->width() - 48;
    const int columns = width < 650 ? 1 : (tiles_.size() <= 4 ? 2 : (width < 1000 ? 3 : 4));
    const int rows = qMax(1, (int(tiles_.size()) + columns - 1) / columns);
    const int imageWidth = (width - 14 * (columns - 1)) / columns;
    const int availableHeight = (scroll_->viewport()->height() - 14 * (rows - 1)) / rows - 124;
    for (auto *tile : tiles_) {
        tile->preview->setFixedHeight(qMax(100, qMin(imageWidth * 9 / 16, availableHeight)));
        // Include wrapped controller/launch labels, rather than compressing them
        // into the one-line size hint of a neighboring tile.
        tile->setFixedHeight(tile->layout()->totalHeightForWidth(imageWidth));
    }
    if (columns == columns_)
        return;
    columns_ = columns;
    while (auto *item = grid_->takeAt(0))
        delete item;
    int i = 0;
    for (auto *tile : tiles_) {
        grid_->addWidget(tile, i / columns, i % columns, Qt::AlignTop);
        ++i;
    }
    for (int c = 0; c < 4; ++c)
        grid_->setColumnStretch(c, c < columns ? 1 : 0);
}
void MonitorWindow::confirmDestroy(const QString &id) {
    if (!model_->desktops().contains(id) || model_->destroying(id))
        return;
    const auto d = model_->desktops()[id];
    QMessageBox dialog(QMessageBox::Warning, "Destroy desktop", QString(), QMessageBox::NoButton,
                       this);
    dialog.setObjectName("destroy-confirmation");
    dialog.setTextFormat(Qt::PlainText);
    dialog.setText("Destroy “" + d.title() + "”?\n\nDesktop ID: " + id);
    dialog.setInformativeText(
        "This closes its applications, loses unsaved work, and removes its private desktop "
        "profile. Any connected controller will be interrupted.\n\nOther desktops and shared "
        "project files are not affected.");
    auto *cancel = dialog.addButton(QMessageBox::Cancel);
    cancel->setObjectName("cancel-destroy");
    auto *destroy = dialog.addButton("Destroy desktop", QMessageBox::DestructiveRole);
    destroy->setObjectName("confirm-destroy");
    dialog.setDefaultButton(cancel);
    dialog.setEscapeButton(cancel);
    dialog.exec();
    if (dialog.clickedButton() == destroy)
        model_->destroyConfirmed(id);
}
void MonitorWindow::resizeEvent(QResizeEvent *event) {
    QMainWindow::resizeEvent(event);
    arrange();
}
void MonitorWindow::changeEvent(QEvent *event) {
    QMainWindow::changeEvent(event);
    if (event->type() == QEvent::WindowStateChange)
        model_->setPreviewEnabled(!isMinimized());
}
void MonitorWindow::closeEvent(QCloseEvent *event) {
    model_->close();
    event->accept();
}
