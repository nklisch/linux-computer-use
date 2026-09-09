#include "preview_widget.h"
#include <QPainter>
#include <QStyle>
#include <QStyleOptionFocusRect>

PreviewWidget::PreviewWidget(QWidget *parent) : QAbstractButton(parent) {
    setFocusPolicy(Qt::StrongFocus);
    setSizePolicy(QSizePolicy::Expanding, QSizePolicy::Expanding);
    setMinimumSize(140, 100);
}
void PreviewWidget::setPreview(const QImage &image, const QString &overlay) {
    image_ = image;
    overlay_ = overlay;
    update();
}
void PreviewWidget::paintEvent(QPaintEvent *) {
    QPainter p(this);
    p.fillRect(rect(), QColor(23, 28, 34));
    if (!image_.isNull()) {
        const auto size = image_.size().scaled(this->size(), Qt::KeepAspectRatio);
        const QRect target(QPoint((width() - size.width()) / 2, (height() - size.height()) / 2),
                           size);
        p.setRenderHint(QPainter::SmoothPixmapTransform);
        p.drawImage(target, image_);
    }
    if (!overlay_.isEmpty()) {
        p.fillRect(rect(), QColor(10, 15, 20, 190));
        p.setPen(Qt::white);
        p.drawText(rect().adjusted(14, 14, -14, -14), Qt::AlignCenter | Qt::TextWordWrap, overlay_);
    }
    if (hasFocus()) {
        QStyleOptionFocusRect option;
        option.initFrom(this);
        option.rect = rect().adjusted(3, 3, -3, -3);
        option.backgroundColor = palette().color(QPalette::Window);
        style()->drawPrimitive(QStyle::PE_FrameFocusRect, &option, &p, this);
    }
}
