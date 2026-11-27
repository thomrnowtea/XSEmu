#pragma once

#include "core/EmulatorController.h"
#include "theme/Theme.h"

#include <functional>

class AppSettings;
class QWidget;
class Services;

// Everything between a Play click and a running game, and how a run ended when it ended badly
// (First launch, Library, Cold-cache prompt and Error states in the design). Each dialog explains
// the cause in a sentence and offers the action that helps.
namespace LaunchErrors {

// The Play path of every Play button: when the gate is free and the title's shaders are cold, the
// cold-cache prompt (Warm up first / Play anyway); otherwise play() at once. A busy gate goes
// straight to play(), whose launch check shows the stop-first dialog.
void confirmPlay(QWidget* window, Services& services, const Game& game, std::function<void()> play,
                 std::function<void()> warmUp);
// True when Play would ask about the shader cache first: never warmed up on this PC, the last
// warm-up for the title stopped part way or failed, or it ran with another run profile. A new XSEmu
// translator build is not known to the launcher, so a finished warm-up with the same profile never asks,
// and nothing asks while the XSEmu warm-up is turned off (AppSettings::xsemuWarmup).
bool shadersCold(Services& services, const Game& game);

// The emulator picker (Settings, About and the missing or too old emulator dialogs): stores the file
// chosen as AppSettings::emulatorPathOverride. False when the user cancelled.
bool chooseEmulator(QWidget* parent, AppSettings& settings);

// Why a launch was refused (EmulatorController::checkLaunch). retry() launches again.
void showLaunchProblem(QWidget* window, Services& services, const Game& game,
                       const EmulatorController::LaunchCheck& check, std::function<void()> retry);

// How a run ended, as text, for the dialog below and for the in-game view's own error screen.
struct ExitReport {
    QString icon;          // Icon name
    Theme::Tone tone = Theme::Tone::Danger;
    QString status;        // the game bar's status line: "Stopped · GPU device removed"
    QString title;         // "The graphics driver reset"
    QString body;          // what happened, in a sentence or three
    QString detailPath;    // the folder with the log and report.json (native separators)
    QString detailNote;    // what that folder holds
    QString openLabel;     // "Open crash report" when report.json exists, else "Open logs"
};
// Empty title for a stop or a normal title exit (nothing to report).
ExitReport describeExit(const EmulatorController::ExitInfo& info);
QString logsFolder(const Game& game);
// report.json when the run left one, else the logs folder.
QString crashReport(const Game& game);
void openCrashReport(const Game& game);

void showExit(QWidget* window, const EmulatorController::ExitInfo& info, std::function<void()> playAgain);

// Free RAM the largest other program holds, summed over its processes ("Chrome", 3.4 GB). Empty
// name when it cannot be read.
struct BiggestApp {
    QString name;
    qint64 bytes = 0;
};
BiggestApp biggestOtherApp();

}
