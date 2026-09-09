#include "monitor_window.h"
#include <QApplication>
#include <QFile>
#include <QJsonDocument>
#include <QMessageBox>
#include <QProcess>
#include <QTemporaryDir>
#include <QtTest>

class MonitorTest : public QObject {
    Q_OBJECT
    QTemporaryDir *root_ = nullptr;
    QProcess fixture_;
    QString socket_;
    QString path(const QString &name) const { return root_->filePath(name); }
    void marker(const QString &name, const QByteArray &text = "yes") {
        QFile file(path(name));
        QVERIFY(file.open(QIODevice::WriteOnly));
        file.write(text);
    }
    QList<QJsonObject> requests() const {
        QFile file(path("requests.jsonl"));
        if (!file.open(QIODevice::ReadOnly))
            return {};
        QList<QJsonObject> result;
        auto lines = file.readAll().split('\n');
        lines.removeLast(); // The fixture may still be appending its current line.
        for (auto line : lines)
            if (!line.isEmpty())
                result.append(QJsonDocument::fromJson(line).object());
        return result;
    }
    int destructions() const {
        int count = 0;
        for (const auto &r : requests())
            count += r["operation"] == "destroy";
        return count;
    }
    int observations() const {
        int count = 0;
        for (const auto &r : requests())
            count += r["request"] == "control";
        return count;
    }
    void shot(MonitorWindow &window, const QString &name) {
        const auto directory = qEnvironmentVariable("LCU_MONITOR_SHOTS");
        if (directory.isEmpty())
            return;
        QDir().mkpath(directory);
        QVERIFY(window.grab().save(directory + "/" + name + ".png"));
    }
private slots:
    void init() {
        root_ = new QTemporaryDir;
        QVERIFY(root_->isValid());
        socket_ = path("fixture.sock");
        const auto binary = qEnvironmentVariable("LCU_MONITOR_FIXTURE");
        QVERIFY2(!binary.isEmpty(),
                 "Set LCU_MONITOR_FIXTURE to the built Rust monitor_fixture executable");
        fixture_.start(binary, {socket_});
        QVERIFY(fixture_.waitForStarted());
        QTRY_VERIFY_WITH_TIMEOUT(QFile::exists(socket_), 5000);
    }
    void cleanup() {
        fixture_.kill();
        fixture_.waitForFinished();
        delete root_;
        root_ = nullptr;
    }
    void nativeOverviewAndReadOnlyClose() {
        MonitorModel model(socket_);
        MonitorWindow window(&model);
        window.show();
        model.start();
        QTRY_COMPARE(model.desktops().size(), 8);
        QTRY_VERIFY(!model.desktops()["fixture-0"].image.isNull());
        QVERIFY(!model.desktops().contains("main"));
        QCOMPARE(model.desktops()["fixture-7"].controllerText(),
                 QString("Desktop unavailable · controller state unknown"));
        QCOMPARE(model.desktops()["fixture-6"].title(), QString("<b>Plain text name</b>"));
        QVERIFY(model.desktops()["fixture-0"].captureAge >= 60000);
        QVERIFY(model.desktops()["fixture-0"].previewError.isEmpty());
        QTRY_VERIFY(!model.desktops()["fixture-6"].image.isNull());
        auto *first = window.findChild<PreviewWidget *>("preview-fixture-0");
        auto *second = window.findChild<PreviewWidget *>("preview-fixture-1");
        QTRY_VERIFY(first->mapTo(&window, QPoint()).y() == second->mapTo(&window, QPoint()).y());
        QVERIFY(first->mapTo(&window, QPoint()).x() != second->mapTo(&window, QPoint()).x());
        QCoreApplication::processEvents();
        for (auto *tile : window.findChildren<DesktopTile *>()) {
            const QRect previewRect(tile->preview->mapTo(tile, QPoint()), tile->preview->size());
            for (auto *text : tile->findChildren<QLabel *>())
                QVERIFY(!previewRect.intersects(QRect(text->mapTo(tile, QPoint()), text->size())));
        }
        shot(window, "eight-light");
        auto *preview = window.findChild<PreviewWidget *>("preview-fixture-0");
        QVERIFY(preview);
        QTest::mouseClick(preview, Qt::LeftButton);
        QCOMPARE(model.selected(), QString("fixture-0"));
        QTRY_VERIFY([&] {
            for (const auto &r : requests())
                if (r["command"].toObject()["args"].toObject()["max_dimension"].toInt() == 1600)
                    return true;
            return false;
        }());
        shot(window, "expanded");
        QTest::mouseClick(window.findChild<QPushButton *>("back"), Qt::LeftButton);
        QVERIFY(model.selected().isEmpty());
        marker("count", "4");
        model.refresh();
        QTRY_COMPARE(model.desktops().size(), 4);
        shot(window, "four-light");
        auto original = window.palette();
        auto dark = original;
        dark.setColor(QPalette::Window, QColor("#232629"));
        dark.setColor(QPalette::WindowText, QColor("#eff0f1"));
        dark.setColor(QPalette::Base, QColor("#232629"));
        dark.setColor(QPalette::Text, QColor("#eff0f1"));
        dark.setColor(QPalette::Button, QColor("#31363b"));
        dark.setColor(QPalette::ButtonText, QColor("#eff0f1"));
        window.setPalette(dark);
        shot(window, "four-dark");
        marker("count", "8");
        model.refresh();
        QTRY_COMPARE(model.desktops().size(), 8);
        QTRY_VERIFY(!model.desktops()["fixture-6"].image.isNull());
        shot(window, "eight-dark");
        window.setPalette(original);
        window.close();
        QTest::qWait(100);
        for (const auto &r : requests()) {
            const auto operation = r["operation"].toString();
            QVERIFY(r["request"] == "hello" || operation == "list" || r["request"] == "control");
            if (r["request"] == "control") {
                QCOMPARE(r["command"].toObject()["command"].toString(), QString("observe"));
                QVERIFY(r["desktop_id"] != "main");
                QCOMPARE(r["command"].toObject()["args"].toObject()["timeout_ms"].toInt(-1), 0);
            }
        }
        QCOMPARE(destructions(), 0);
    }
    void previewAndListingFailuresRetainEvidence() {
        MonitorModel model(socket_);
        model.start();
        QTRY_VERIFY(model.desktops().contains("fixture-0") &&
                    !model.desktops()["fixture-0"].image.isNull());
        model.setPreviewEnabled(false);
        marker("fail-observe");
        model.setPreviewEnabled(true);
        model.select("fixture-0");
        QTRY_VERIFY(!model.desktops()["fixture-0"].previewError.isEmpty());
        QVERIFY(!model.desktops()["fixture-0"].image.isNull());
        QVERIFY(model.desktops()["fixture-0"].available);
        marker("fail-list");
        model.refresh();
        QTRY_VERIFY(!model.desktops()["fixture-0"].infoCurrent);
        QCOMPARE(model.desktops().size(), 8);
        QVERIFY(model.desktops()["fixture-0"].controllerText().contains("unknown"));
        QFile::remove(path("fail-list"));
        QFile::remove(path("fail-observe"));
        model.refresh();
        QTRY_VERIFY(model.desktops()["fixture-0"].infoCurrent);
        QTRY_VERIFY(model.desktops()["fixture-0"].previewError.isEmpty());
        model.setPreviewEnabled(false);
        const int before = observations();
        QTest::qWait(600);
        QCOMPARE(observations(), before);
    }
    void cancellationDoesNotQueueOrReplaceNewView() {
        marker("delay-observe");
        MonitorModel model(socket_);
        model.start();
        QTRY_COMPARE(model.desktops().size(), 8);
        QTest::qWait(500);
        QCOMPARE(observations(), 2);
        model.select("fixture-4");
        QTRY_COMPARE(observations(), 3);
        model.select("fixture-5");
        QTRY_COMPARE(observations(), 4);
        QFile::remove(path("delay-observe"));
        model.select("fixture-6");
        QTRY_VERIFY(!model.desktops()["fixture-6"].image.isNull());
        QVERIFY(model.desktops()["fixture-4"].image.isNull());
        QVERIFY(model.desktops()["fixture-5"].image.isNull());
        QCOMPARE(model.selected(), QString("fixture-6"));
    }
    void slowPreviewsDoNotStarveOtherDesktops() {
        marker("delay-observe");
        MonitorModel model(socket_);
        model.start();
        QTRY_COMPARE(model.desktops().size(), 8);
        QTRY_VERIFY_WITH_TIMEOUT(
            [&] {
                QSet<QString> observed;
                for (const auto &r : requests())
                    if (r["request"] == "control")
                        observed.insert(r["desktop_id"].toString());
                return observed.size() == 7; // The eighth desktop is unavailable.
            }(),
            12000);
    }
    void disconnectedImageAgesKeepAdvancing() {
        MonitorModel model(socket_);
        MonitorWindow window(&model);
        window.show();
        model.start();
        QTRY_VERIFY(model.desktops().contains("fixture-0") &&
                    !model.desktops()["fixture-0"].image.isNull());
        fixture_.kill();
        fixture_.waitForFinished();
        QTRY_VERIFY(!model.connected());
        auto *tile = window.findChild<PreviewWidget *>("preview-fixture-0")->parentWidget();
        auto age = [&] {
            for (auto *text : tile->findChildren<QLabel *>())
                if (text->text().startsWith("Received "))
                    return text->text();
            return QString();
        };
        const auto before = age();
        QVERIFY(!before.isEmpty());
        QTRY_VERIFY_WITH_TIMEOUT(age() != before, 3000);
        QVERIFY(!model.desktops()["fixture-0"].image.isNull());
    }
    void nativeConfirmationIsRequiredAndSingleShot() {
        MonitorModel model(socket_);
        MonitorWindow window(&model);
        window.show();
        model.start();
        QTRY_COMPARE(model.desktops().size(), 8);
        auto *tile = window.findChild<PreviewWidget *>("preview-fixture-0")->parentWidget();
        auto *desktop = qobject_cast<DesktopTile *>(tile);
        QVERIFY(desktop);
        QTimer::singleShot(0, [&] {
            auto *dialog = window.findChild<QMessageBox *>("destroy-confirmation");
            QVERIFY(dialog);
            QVERIFY(dialog->text().contains("fixture-0"));
            QCOMPARE(dialog->defaultButton()->objectName(), QString("cancel-destroy"));
            QTest::keyClick(dialog, Qt::Key_Return); // Default keyboard action must cancel.
        });
        desktop->destroyRequested("fixture-0");
        QCOMPARE(destructions(), 0);
        QTimer::singleShot(0, [&] {
            auto *dialog = window.findChild<QMessageBox *>("destroy-confirmation");
            QVERIFY(dialog);
            QTest::mouseClick(dialog->findChild<QPushButton *>("confirm-destroy"), Qt::LeftButton);
        });
        desktop->destroyRequested("fixture-0");
        model.destroyConfirmed("fixture-0"); // Busy suppression, not a second operation.
        QTRY_COMPARE(destructions(), 1);
        QTRY_VERIFY(!model.desktops().contains("fixture-0"));
        QCOMPARE(model.desktops().size(), 7);
        model.destroyConfirmed("main");
        QTest::qWait(100);
        QCOMPARE(destructions(), 1);
        window.close();
        QTest::qWait(100);
        QCOMPARE(destructions(), 1);
    }
    void missingSocketDoesNotStartOrFallback() {
        MonitorModel model(path("missing.sock"));
        model.start();
        QTRY_VERIFY(model.message().contains("No daemon was started"));
        QVERIFY(!model.connected());
        QVERIFY(model.desktops().isEmpty());
        QCOMPARE(requests().size(), 0);
    }
};
QTEST_MAIN(MonitorTest)
#include "monitor_test.moc"
