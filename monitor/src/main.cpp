#include "monitor_window.h"
#include <QApplication>
#include <QCommandLineParser>
#include <QFileInfo>
#include <cstdio>

int main(int argc, char **argv) {
    QApplication app(argc, argv);
    QCoreApplication::setApplicationName("lcu-monitor");
    QCoreApplication::setApplicationVersion("0.2.0");
    QGuiApplication::setDesktopFileName("org.nklisch.LcuMonitor");
    QCommandLineParser parser;
    parser.setApplicationDescription(
        "Read-only agent desktop monitor. Never starts or restarts LCU.");
    parser.addHelpOption();
    parser.addVersionOption();
    QCommandLineOption socket("socket", "Existing LCU daemon socket (absolute path)", "path");
    parser.addOption(socket);
    parser.process(app);
    const auto path = parser.value(socket);
    if (path.isEmpty() || !QFileInfo(path).isAbsolute()) {
        std::fprintf(stderr,
                     "Pass --socket with an absolute daemon socket path, or use lcu monitor.\n");
        return 2;
    }
    MonitorModel model(path);
    MonitorWindow window(&model);
    window.show();
    model.start();
    return app.exec();
}
