#pragma once

#include <QList>
#include <QPair>
#include <QString>
#include <QStringList>
#include <QVariantMap>

class AppSettings;

// Settings profiles people share: a small versioned JSON file (*.xsemu-settings) or a share code
// (XSEMU1: + base64url of the zlib-compressed compact JSON) that fits in a chat message.
//
//   SettingsProfile::Profile p = SettingsProfile::fromGame(settings, game.titleId(), game.title());
//   const QString code = SettingsProfile::toCode(p);
//   const SettingsProfile::Parsed in = SettingsProfile::parse(code.toUtf8());
//   if (in.ok()) SettingsProfile::apply(in, settings, SettingsProfile::Target::Game, titleId);
//
// Only the keys in keys() ever leave the PC: it is an allow-list, so a setting added later stays
// private until someone adds it here. Paths, API keys, the library, play stats, warm-up history,
// window geometry and the D3D12 adapter (a GPU of this PC) are never in it. The global run profile is,
// without its lines whose value is an absolute path; importing one keeps this PC's own path lines.
namespace SettingsProfile {

inline constexpr const char* kSchema = "xsemu-settings";
inline constexpr int kVersion = 1;
inline constexpr const char* kCodePrefix = "XSEMU1:";
inline constexpr const char* kFileSuffix = "xsemu-settings";
inline constexpr int kNameMax = 60;
inline constexpr int kNoteMax = 280;

enum class Target { Game, Defaults };

// One shareable key. The profile key is the store's own key: the game override key
// (AppSettings::gameOverrides) and, for xcl.toml keys, the LaunchConfig::xclKeys() name.
struct KeySpec {
    enum class Kind { Choice, Bool, DisplayMode, Runner, RunProfile };
    QString key;
    QString label;
    Kind kind;
    QList<QPair<QVariant, QString>> choices; // Choice: every value the launcher accepts, with its label
    bool game = true;                        // applies to one game
    bool defaults = true;                    // applies as a launcher default
    QString unavailable;                     // non-empty: the launcher never applies it, and why
};
const QList<KeySpec>& keys();
const KeySpec* find(const QString& key);
// "Series S", "1920×1080 · 120 Hz", "On", "1.5×", "Kelvaroq", "17 variables"; "Not set" for an invalid value.
QString valueLabel(const QString& key, const QVariant& value);

struct Profile {
    QString name;
    QString note;
    QString launcherVersion; // where it was made (info only)
    QString gpuVendor;
    QString gpuModel;
    QString titleId;         // the game it was made from; empty for launcher defaults
    QString title;
    QVariantMap values;      // key -> canonical value; "runner" holds the runner id
    QString runnerVersion;   // the pinned version of values["runner"] where it was made
};

// The game's own overrides, or every launcher default Settings manages. Name, note, version and GPU
// are left for the caller.
Profile fromGame(const AppSettings& settings, const QString& titleId, const QString& title);
Profile fromDefaults(const AppSettings& settings);

QByteArray toJson(const Profile& profile, bool compact = false);
QString toCode(const Profile& profile);

struct Note {
    QString key;
    QString value; // as the profile wrote it, for display
    QString text;
};

// A run profile line's value names a place on one PC: C:\..., \\server\..., /..., ~/...
bool looksLikePath(const QString& value);

struct Parsed {
    Profile profile;             // valid values only
    QString error;               // non-empty: nothing can be imported
    QStringList ignored;         // keys this launcher doesn't know
    QList<Note> rejected;        // values that are malformed, out of range or unknown; dropped
    QList<Note> forbidden;       // valid values the launcher never applies (online, unavailable features)
    QVariantMap forbiddenValues; // the same, by key, canonical
    bool ok() const { return error.isEmpty(); }
};
// A file's bytes, or text holding a share code (line breaks inside it are ignored).
Parsed parse(const QByteArray& input);

struct Line {
    enum class Kind { Change, Same, NotApplied };
    Kind kind = Kind::Same;
    QString key;
    QString label;
    QString from;
    QString to;
    QString reason; // NotApplied: why
    QVariant value;
};
// What applying would do to the game (titleId) or to the launcher defaults, in keys() order.
QList<Line> preview(const Parsed& parsed, const AppSettings& settings, Target target, const QString& titleId = {});
// Writes the Change lines of preview(); returns how many settings changed.
int apply(const Parsed& parsed, AppSettings& settings, Target target, const QString& titleId = {});

}
