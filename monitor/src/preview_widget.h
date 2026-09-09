#pragma once
#include <QAbstractButton>
#include <QImage>

// Display only: the button can open a larger view, never forward input.
class PreviewWidget : public QAbstractButton {
    Q_OBJECT
public:
    explicit PreviewWidget(QWidget *parent = nullptr);
    void setPreview(const QImage &image, const QString &overlay);
    QSize sizeHint() const override { return {480, 270}; }

protected:
    void paintEvent(QPaintEvent *) override;

private:
    QImage image_;
    QString overlay_;
};
