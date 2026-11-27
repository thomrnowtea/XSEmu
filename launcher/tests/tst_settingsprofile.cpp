#include "AppSettings.h"
#include "LaunchConfig.h"
#include "Runner.h"
#include "SettingsProfile.h"

#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>
#include <QTemporaryDir>
#include <QTest>
#include <QtEndian>

#include <algorithm>

using namespace SettingsProfile;

namespace {

const QString kTitle = QStringLiteral("CoastlineDrive_8wekyb3d8bbwe");

QString code(const QByteArray& json)
{
    return QLatin1String(kCodePrefix) + QString::fromLatin1(qCompress(json).toBase64(QByteArray::Base64UrlEncoding | QByteArray::OmitTrailingEquals));
}

QByteArray profileJson(const QJsonObject& settings, const QJsonObject& extra = {})
{
    QJsonObject o{{QStringLiteral("schema"), QStringLiteral("xsemu-settings")}, {QStringLiteral("version"), 1}, {QStringLiteral("settings"), settings}};
    for (auto it = extra.constBegin(); it != extra.constEnd(); ++it)
        o.insert(it.key(), it.value());
    return QJsonDocument(o).toJson(QJsonDocument::Compact);
}

QByteArray decodeCode(const QString& c)
{
    return qUncompress(QByteArray::fromBase64(c.mid(int(qstrlen(kCodePrefix))).toLatin1(), QByteArray::Base64UrlEncoding));
}

void tuneGame(AppSettings& s)
{
    s.setGameOverride(kTitle, QStringLiteral("device_type"), QStringLiteral("anaconda"));
    s.setGameOverride(kTitle, QStringLiteral("display_mode"), QStringLiteral("1920x1080"));
    s.setGameOverride(kTitle, QStringLiteral("raytracing"), QStringLiteral("tier1_1"));
    s.setGameOverride(kTitle, QStringLiteral("overlay"), true);
    s.setGameOverride(kTitle, QStringLiteral("mouse_sensitivity"), 1.5);
    s.setGameOverride(kTitle, QStringLiteral("log_level"), QStringLiteral("debug"));
    s.setGameOverride(kTitle, QStringLiteral("runner"), QStringLiteral("kelvaroq"));
}

Profile typicalProfile(const AppSettings& s)
{
    Profile p = fromGame(s, kTitle, QStringLiteral("Coastline Drive"));
    p.note = QStringLiteral("Locked 60 in the city with these; Series X needs 32 GB RAM.");
    p.launcherVersion = QStringLiteral("0.1.0");
    p.gpuVendor = QStringLiteral("NVIDIA");
    p.gpuModel = QStringLiteral("NVIDIA GeForce RTX 5080");
    return p;
}

}

class TestSettingsProfile : public QObject {
    Q_OBJECT
private slots:
    void allowListIsBuiltOnLaunchConfig();
    void roundTripThroughFileAndCode();
    void defaultsRoundTrip();
    void codeIsShortEnoughToPaste();
    void nothingPrivateIsExported();
    void malformedInput_data();
    void malformedInput();
    void unknownKeysAreIgnoredAndListed();
    void badValuesAreRejectedPerKey();
    void unknownRunnerIsIgnored();
    void forbiddenValuesAreNeverApplied();
    void applyToGameWritesTheGameOnly();
    void applyAsDefaultsWritesTheDefaultsOnly();
    void environmentOverridesAreNotApplied();
    void runProfileTravelsWithoutPaths();
};

void TestSettingsProfile::allowListIsBuiltOnLaunchConfig()
{
    QStringList xcl;
    for (const LaunchConfig::XclKey& k : LaunchConfig::xclKeys())
        xcl << QLatin1String(k.key);
    // every xcl.toml key Settings writes is shareable except the D3D12 adapter, a GPU of this PC
    for (const QString& key : xcl) {
        if (key == QLatin1String("d3d12_adapter"))
            QVERIFY(!find(key));
        else
            QVERIFY2(find(key), qPrintable(key));
    }
    // every per-game override key the launcher writes is known
    for (const char* key : {"overlay", "runner", "alwaysWarmUp", "startIn", "lowLatency"})
        QVERIFY2(find(QLatin1String(key)), key);
    QVERIFY(!find(QStringLiteral("steamGridDbKey")));
    QVERIFY(!find(QStringLiteral("d3d12_adapter")));
}

void TestSettingsProfile::roundTripThroughFileAndCode()
{
    QTemporaryDir dir;
    AppSettings from(dir.filePath(QStringLiteral("a.ini")));
    tuneGame(from);
    const Profile p = typicalProfile(from);
    QCOMPARE(p.values.value(QStringLiteral("device_type")).toString(), QStringLiteral("anaconda"));
    QCOMPARE(p.values.value(QStringLiteral("runner")).toString(), QStringLiteral("kelvaroq"));
    QCOMPARE(p.runnerVersion, Runners::find(QStringLiteral("kelvaroq"))->pinnedVersion());

    for (const QByteArray& input : {toJson(p), toCode(p).toUtf8()}) {
        const Parsed in = parse(input);
        QVERIFY2(in.ok(), qPrintable(in.error));
        QVERIFY(in.ignored.isEmpty());
        QVERIFY(in.rejected.isEmpty());
        QVERIFY(in.forbidden.isEmpty());
        QCOMPARE(in.profile.values, p.values);
        QCOMPARE(in.profile.name, p.name);
        QCOMPARE(in.profile.note, p.note);
        QCOMPARE(in.profile.launcherVersion, p.launcherVersion);
        QCOMPARE(in.profile.gpuModel, p.gpuModel);
        QCOMPARE(in.profile.gpuVendor, p.gpuVendor);
        QCOMPARE(in.profile.runnerVersion, p.runnerVersion);
        QCOMPARE(in.profile.titleId, kTitle);

        AppSettings to(dir.filePath(QStringLiteral("b%1.ini").arg(input.size())));
        QCOMPARE(apply(in, to, Target::Game, kTitle), 7);
        QCOMPARE(to.gameOverrides(kTitle), from.gameOverrides(kTitle));
        QCOMPARE(fromGame(to, kTitle, QStringLiteral("Coastline Drive")).values, p.values);
        QCOMPARE(apply(in, to, Target::Game, kTitle), 0);
    }
    // a code pasted with chat text around it and a line break in the middle
    QString c = toCode(p);
    c.insert(40, QStringLiteral("\r\n"));
    QVERIFY(parse(QStringLiteral("try mine: %1 thanks").arg(c).toUtf8()).ok());
    QVERIFY(parse((c + QLatin1Char('.')).toUtf8()).ok());
}

void TestSettingsProfile::defaultsRoundTrip()
{
    QTemporaryDir dir;
    AppSettings from(dir.filePath(QStringLiteral("a.ini")));
    from.setDefaultDeviceType(QStringLiteral("anaconda"));
    from.setOverlayDefault(true);
    from.setCrashDumps(false);
    from.setDefaultRunner(QStringLiteral("victory-dexnom"));
    from.setXclDefault(QStringLiteral("display_mode"), QStringLiteral("2560x1440@120"));
    from.setXclDefault(QStringLiteral("keyboard_mouse"), false);
    from.setXclDefault(QStringLiteral("mouse_sensitivity"), 0.75);
    from.setXclDefault(QStringLiteral("present"), QStringLiteral("off"));
    const Profile p = fromDefaults(from);
    QVERIFY(p.titleId.isEmpty());
    QCOMPARE(p.values.value(QStringLiteral("offline")), QVariant(true));
    QCOMPARE(p.values.value(QStringLiteral("store")).toString(), QStringLiteral("offline-error"));
    // ini values come back as strings: the profile carries the TOML types
    QCOMPARE(p.values.value(QStringLiteral("keyboard_mouse")), QVariant(false));
    QCOMPARE(p.values.value(QStringLiteral("mouse_sensitivity")).toDouble(), 0.75);

    const Parsed in = parse(toCode(p).toUtf8());
    QVERIFY2(in.ok(), qPrintable(in.error));
    QCOMPARE(in.profile.values, p.values);
    AppSettings to(dir.filePath(QStringLiteral("b.ini")));
    QVERIFY(apply(in, to, Target::Defaults) > 0);
    QCOMPARE(fromDefaults(to).values, p.values);
    QCOMPARE(to.defaultRunner(), QStringLiteral("victory-dexnom"));
    QCOMPARE(to.xclDefault(QStringLiteral("display_mode")).toString(), QStringLiteral("2560x1440@120"));
}

void TestSettingsProfile::codeIsShortEnoughToPaste()
{
    QTemporaryDir dir;
    AppSettings s(dir.filePath(QStringLiteral("a.ini")));
    tuneGame(s);
    const QString c = toCode(typicalProfile(s));
    qInfo("typical share code, %d characters: %s", int(c.size()), qPrintable(c));
    QVERIFY(c.startsWith(QLatin1String("XSEMU1:")));
    // a Discord message holds 2000 characters; a typical profile stays far below
    QVERIFY2(c.size() <= 600, qPrintable(QString::number(c.size())));
    for (QChar ch : c.mid(7))
        QVERIFY(ch.isLetterOrNumber() || ch == QLatin1Char('-') || ch == QLatin1Char('_'));

    AppSettings d(dir.filePath(QStringLiteral("d.ini")));
    Profile all = fromDefaults(d);
    all.note = QString(kNoteMax, QLatin1Char('n'));
    QVERIFY(toCode(all).size() <= 1000);
}

void TestSettingsProfile::nothingPrivateIsExported()
{
    QTemporaryDir dir;
    AppSettings s(dir.filePath(QStringLiteral("a.ini")));
    tuneGame(s);
    const QStringList planted{
        QStringLiteral("/home/alice/Games"), QStringLiteral("D:\\XboxGames"), QStringLiteral("sgdb-0123456789abcdef"),
        QStringLiteral("/opt/xsemu/XSEmu.exe"), QStringLiteral("/mnt/fast/dxil"), QStringLiteral("/tmp/xsemu.busy"),
        QStringLiteral("GeForce RTX 5080 #2"), QStringLiteral("LUID-0000-ABCD"), QStringLiteral("C:\\Users\\alice\\dumps"),
        QStringLiteral("tok-secret-42"), QStringLiteral("SignalGarden_1x2y3z"), QStringLiteral("Signal Garden"), QStringLiteral("4096"),
        QStringLiteral("C:\\Users\\alice\\skip.txt"),
    };
    s.setRunProfile({QStringLiteral("XCL_LOOP_GUARD=1024"), QStringLiteral("XCL_SKIP_SHADERS_FILE=") + planted[13]});
    s.setGameFolders({planted[0], planted[1]});
    s.setSteamGridDbKey(planted[2]);
    s.setArtLookups(true);
    s.setEmulatorPathOverride(planted[3]);
    s.setShaderCacheDirOverride(planted[4]);
    s.setBusyMarker(planted[5]);
    s.setXclDefault(QStringLiteral("d3d12_adapter"), planted[6]);
    s.setGameOverride(kTitle, QStringLiteral("d3d12_adapter"), planted[7]);
    s.setGameOverride(kTitle, QStringLiteral("crashDumpDir"), planted[8]);
    s.setGameOverride(kTitle, QStringLiteral("apiToken"), planted[9]);
    s.setGameOverride(planted[10], QStringLiteral("overlay"), true);
    s.setLaunchNeedMb(4096);
    s.addPlayTime(kTitle, 7777, QDateTime::currentDateTimeUtc());
    s.setLastRun(kTitle, {{QStringLiteral("result"), QStringLiteral("Stopped by you")}, {QStringLiteral("heldPeakBytes"), qint64(9234567890)}});
    WarmupRun run;
    run.titleId = planted[10];
    run.titleName = planted[11];
    run.total = 41906;
    s.addRecentWarmup(run);
    s.setLibrarySort(QStringLiteral("size"));

    const QList<Profile> profiles{typicalProfile(s), fromDefaults(s)};
    for (const Profile& p : profiles) {
        const QByteArray file = toJson(p);
        const QByteArray decoded = decodeCode(toCode(p));
        QVERIFY(!decoded.isEmpty());
        for (const QByteArray& out : {file, decoded}) {
            for (const QString& secret : planted)
                QVERIFY2(!out.contains(secret.toUtf8()), qPrintable(secret + QStringLiteral(" leaked: ") + QString::fromUtf8(out)));
            for (const char* key : {"d3d12_adapter", "crashDumpDir", "apiToken", "steamGridDbKey", "7777", "41906", "lastRun", "folders", "geometry"})
                QVERIFY2(!out.contains(key), key);
            QVERIFY(!out.contains("/home/") && !out.contains(":\\\\"));
        }
    }
}

void TestSettingsProfile::malformedInput_data()
{
    QTest::addColumn<QByteArray>("input");
    QTest::addColumn<QString>("expect");
    const QByteArray good = profileJson({{QStringLiteral("overlay"), true}});
    const QString goodCode = code(good);
    QTest::newRow("empty") << QByteArray() << QStringLiteral("isn't an XSEmu settings code");
    QTest::newRow("random text") << QByteArray("hello there") << QStringLiteral("isn't an XSEmu settings code");
    QTest::newRow("bad base64 character") << (goodCode.left(20) + QStringLiteral("$%") + goodCode.mid(20)).toUtf8() << QStringLiteral("damaged");
    QString badLength = goodCode;
    while ((badLength.size() - 7) % 4 != 1)
        badLength += QLatin1Char('A');
    QTest::newRow("bad base64 length") << badLength.toUtf8() << QStringLiteral("incomplete or damaged");
    QTest::newRow("truncated code") << goodCode.left(goodCode.size() - 6).toUtf8() << QStringLiteral("incomplete or damaged");
    QTest::newRow("prefix only") << QByteArray("XSEMU1:") << QStringLiteral("incomplete or damaged");
    QByteArray notZlib = qCompress(good);
    notZlib[8] = char(notZlib[8] ^ 0x5A);
    notZlib[9] = char(notZlib[9] ^ 0x5A);
    QTest::newRow("bad zlib") << (QByteArray(kCodePrefix) + notZlib.toBase64(QByteArray::Base64UrlEncoding)) << QStringLiteral("incomplete or damaged");
    QByteArray huge(8, '\0');
    qToBigEndian<quint32>(0x7fffffff, huge.data());
    QTest::newRow("zlib bomb header") << (QByteArray(kCodePrefix) + huge.toBase64(QByteArray::Base64UrlEncoding)) << QStringLiteral("incomplete or damaged");
    QTest::newRow("code holding bad JSON") << code("{\"schema\": \"xsemu-settings\", ").toUtf8() << QStringLiteral("isn't readable");
    QTest::newRow("bad JSON file") << QByteArray("{\"schema\": \"xsemu-settings\", \"version\": 1,") << QStringLiteral("This file isn't readable");
    QTest::newRow("JSON array") << QByteArray("[1, 2]") << QStringLiteral("isn't an XSEmu settings code");
    QTest::newRow("wrong schema") << QByteArray(R"({"schema": "xcl-config", "version": 1, "settings": {}})") << QStringLiteral("isn't an XSEmu settings profile");
    QTest::newRow("no schema") << QByteArray(R"({"version": 1, "settings": {}})") << QStringLiteral("isn't an XSEmu settings profile");
    QTest::newRow("newer version") << QByteArray(R"({"schema": "xsemu-settings", "version": 2, "settings": {}})") << QStringLiteral("newer launcher. Update XSEmu");
    QTest::newRow("newer code") << QByteArray("XSEMU2:abcdef") << QStringLiteral("newer launcher. Update XSEmu");
    QTest::newRow("newer version in a code") << code(R"({"schema": "xsemu-settings", "version": 7})").toUtf8() << QStringLiteral("newer launcher");
    QTest::newRow("version as text") << QByteArray(R"({"schema": "xsemu-settings", "version": "1", "settings": {}})") << QStringLiteral("\"version\" must be a whole number");
    QTest::newRow("fractional version") << QByteArray(R"({"schema": "xsemu-settings", "version": 1.5})") << QStringLiteral("\"version\" must be a whole number");
    QTest::newRow("settings as array") << QByteArray(R"({"schema": "xsemu-settings", "version": 1, "settings": ["overlay"]})") << QStringLiteral("\"settings\" must be an object");
    QTest::newRow("too large") << QByteArray(300 * 1024, ' ') << QStringLiteral("too large");
}

void TestSettingsProfile::malformedInput()
{
    QFETCH(QByteArray, input);
    QFETCH(QString, expect);
    const Parsed in = parse(input);
    QVERIFY(!in.ok());
    QVERIFY2(in.error.contains(expect), qPrintable(in.error));
    QVERIFY(in.profile.values.isEmpty());
    QTemporaryDir dir;
    AppSettings s(dir.filePath(QStringLiteral("a.ini")));
    QCOMPARE(apply(in, s, Target::Defaults), 0);
}

void TestSettingsProfile::unknownKeysAreIgnoredAndListed()
{
    const Parsed in = parse(profileJson({{QStringLiteral("overlay"), true}, {QStringLiteral("fsr_sharpness"), 0.4}, {QStringLiteral("d3d12_adapter"), QStringLiteral("warp")}},
                                        {{QStringLiteral("theme"), QStringLiteral("dark")}}));
    QVERIFY(in.ok());
    QCOMPARE(in.profile.values, (QVariantMap{{QStringLiteral("overlay"), true}}));
    QStringList ignored = in.ignored;
    ignored.sort();
    QCOMPARE(ignored, (QStringList{QStringLiteral("d3d12_adapter"), QStringLiteral("fsr_sharpness"), QStringLiteral("theme")}));
}

void TestSettingsProfile::badValuesAreRejectedPerKey()
{
    const Parsed in = parse(profileJson({
        {QStringLiteral("overlay"), QStringLiteral("yes")},          // wrong type
        {QStringLiteral("keyboard_mouse"), 1},                         // wrong type
        {QStringLiteral("device_type"), QStringLiteral("scarlett")},   // unknown enum
        {QStringLiteral("mouse_sensitivity"), 9.0},                    // out of range
        {QStringLiteral("display_mode"), QStringLiteral("1920x1080@75")}, // XSEmu takes 60 or 120 Hz
        {QStringLiteral("raytracing"), QStringLiteral("TIER1_0")},     // case is forgiven
        {QStringLiteral("log_level"), QJsonObject{}},                  // wrong type
    }, {{QStringLiteral("name"), 42}}));
    QVERIFY(in.ok());
    QCOMPARE(in.profile.values, (QVariantMap{{QStringLiteral("raytracing"), QStringLiteral("tier1_0")}}));
    QStringList keys;
    for (const Note& n : in.rejected) {
        keys << n.key;
        QVERIFY(!n.text.isEmpty());
    }
    keys.sort();
    QCOMPARE(keys, (QStringList{QStringLiteral("device_type"), QStringLiteral("display_mode"), QStringLiteral("keyboard_mouse"), QStringLiteral("log_level"),
                                QStringLiteral("mouse_sensitivity"), QStringLiteral("name"), QStringLiteral("overlay")}));
    QVERIFY(in.profile.name.isEmpty());
}

void TestSettingsProfile::unknownRunnerIsIgnored()
{
    const Parsed in = parse(profileJson({{QStringLiteral("overlay"), false}}, {{QStringLiteral("runner"), QJsonObject{{QStringLiteral("id"), QStringLiteral("cemu-x")}}}}));
    QVERIFY(in.ok());
    QVERIFY(!in.profile.values.contains(QStringLiteral("runner")));
    QCOMPARE(in.rejected.size(), 1);
    QCOMPARE(in.rejected.first().key, QStringLiteral("runner"));
    QVERIFY(in.rejected.first().text.contains(QLatin1String("unknown runner")));
}

void TestSettingsProfile::forbiddenValuesAreNeverApplied()
{
    QTemporaryDir dir;
    AppSettings s(dir.filePath(QStringLiteral("a.ini")));
    const Parsed in = parse(profileJson({
        {QStringLiteral("offline"), false},
        {QStringLiteral("store"), QStringLiteral("not-available")},
        {QStringLiteral("startIn"), QStringLiteral("fullscreen")},
        {QStringLiteral("lowLatency"), true},
        {QStringLiteral("overlay"), true},
    }));
    QVERIFY(in.ok());
    QCOMPARE(in.forbidden.size(), 4);
    QCOMPARE(in.profile.values, (QVariantMap{{QStringLiteral("overlay"), true}}));

    for (Target t : {Target::Game, Target::Defaults}) {
        int notApplied = 0;
        for (const Line& l : preview(in, s, t, kTitle)) {
            if (l.key != QLatin1String("overlay")) {
                QCOMPARE(int(l.kind), int(Line::Kind::NotApplied));
                QVERIFY(!l.reason.isEmpty());
                ++notApplied;
            }
        }
        QCOMPARE(notApplied, 4);
        QCOMPARE(apply(in, s, t, kTitle), 1);
    }
    QVERIFY(s.offline());
    QVERIFY(s.storeOfflineError());
    QCOMPARE(s.gameOverrides(kTitle), (QVariantMap{{QStringLiteral("overlay"), true}}));
    QCOMPARE(LaunchConfig::runEnvironment(QProcessEnvironment(), s, kTitle).value(QStringLiteral("XCL_NETWORK")), QStringLiteral("0"));

    // even a stored online setting never travels as anything but a line the importer refuses
    s.setOffline(false);
    const Parsed shared = parse(toJson(fromDefaults(s)));
    QVERIFY(shared.forbiddenValues.contains(QStringLiteral("offline")));
    AppSettings fresh(dir.filePath(QStringLiteral("b.ini")));
    apply(shared, fresh, Target::Defaults);
    QVERIFY(fresh.offline());
}

void TestSettingsProfile::applyToGameWritesTheGameOnly()
{
    QTemporaryDir dir;
    AppSettings s(dir.filePath(QStringLiteral("a.ini")));
    const Parsed in = parse(profileJson({{QStringLiteral("device_type"), QStringLiteral("anaconda")}, {QStringLiteral("overlay"), true},
                                         {QStringLiteral("display_mode"), QStringLiteral("host")}, {QStringLiteral("crash_dumps"), false},
                                         {QStringLiteral("alwaysWarmUp"), true}},
                                        {{QStringLiteral("runner"), QJsonObject{{QStringLiteral("id"), QStringLiteral("kelvaroq")}, {QStringLiteral("version"), QStringLiteral("v0.0.1")}}}}));
    QVERIFY(in.ok());
    const QList<Line> lines = preview(in, s, Target::Game, kTitle);
    QCOMPARE(lines.size(), 6);
    for (const Line& l : lines) {
        if (l.key == QLatin1String("device_type")) {
            QCOMPARE(l.from, QStringLiteral("Series S"));
            QCOMPARE(l.to, QStringLiteral("Series X"));
        }
        if (l.key == QLatin1String("crash_dumps"))
            QCOMPARE(int(l.kind), int(Line::Kind::NotApplied));
    }
    QCOMPARE(apply(in, s, Target::Game, kTitle), 5);
    const QVariantMap own = s.gameOverrides(kTitle);
    QCOMPARE(own.value(QStringLiteral("device_type")).toString(), QStringLiteral("anaconda"));
    QCOMPARE(own.value(QStringLiteral("runner")).toString(), QStringLiteral("kelvaroq"));
    QCOMPARE(own.value(QStringLiteral("alwaysWarmUp")), QVariant(true));
    QCOMPARE(LaunchConfig::deviceType(s, kTitle), QStringLiteral("anaconda"));
    QCOMPARE(LaunchConfig::titleValues(s, kTitle).value(QStringLiteral("display_mode")).toString(), QStringLiteral("host"));
    // the defaults are untouched, and so is every other game
    QCOMPARE(s.defaultDeviceType(), QStringLiteral("lockhart"));
    QVERIFY(!s.overlayDefault());
    QVERIFY(s.crashDumps());
    QCOMPARE(s.defaultRunner(), QStringLiteral("xsemu"));
    QVERIFY(!s.xclDefault(QStringLiteral("display_mode")).isValid());
    QVERIFY(s.gameOverrides(QStringLiteral("Other")).isEmpty());
    QCOMPARE(apply(in, s, Target::Game, {}), 0);
}

void TestSettingsProfile::applyAsDefaultsWritesTheDefaultsOnly()
{
    QTemporaryDir dir;
    AppSettings s(dir.filePath(QStringLiteral("a.ini")));
    const Parsed in = parse(profileJson({{QStringLiteral("device_type"), QStringLiteral("anaconda")}, {QStringLiteral("overlay"), true},
                                         {QStringLiteral("display_mode"), QStringLiteral("host")}, {QStringLiteral("crash_dumps"), false},
                                         {QStringLiteral("alwaysWarmUp"), true}, {QStringLiteral("mouse_sensitivity"), 2}},
                                        {{QStringLiteral("runner"), QJsonObject{{QStringLiteral("id"), QStringLiteral("kelvaroq")}}}}));
    QVERIFY(in.ok());
    QCOMPARE(apply(in, s, Target::Defaults), 6);
    QCOMPARE(s.defaultDeviceType(), QStringLiteral("anaconda"));
    QVERIFY(s.overlayDefault());
    QVERIFY(!s.crashDumps());
    QCOMPARE(s.defaultRunner(), QStringLiteral("kelvaroq"));
    QCOMPARE(s.xclDefault(QStringLiteral("display_mode")).toString(), QStringLiteral("host"));
    QCOMPARE(LaunchConfig::typed(QStringLiteral("mouse_sensitivity"), s.xclDefault(QStringLiteral("mouse_sensitivity"))).toDouble(), 2.0);
    QVERIFY(s.gameOverrides(kTitle).isEmpty());
    // the game follows the new defaults without an override of its own
    QCOMPARE(LaunchConfig::deviceType(s, kTitle), QStringLiteral("anaconda"));
}

void TestSettingsProfile::environmentOverridesAreNotApplied()
{
    QTemporaryDir dir;
    AppSettings s(dir.filePath(QStringLiteral("a.ini")));
    qputenv("XCL_RAYTRACING", "off");
    const Parsed in = parse(profileJson({{QStringLiteral("raytracing"), QStringLiteral("tier1_1")}}));
    QCOMPARE(apply(in, s, Target::Defaults), 0);
    const QList<Line> lines = preview(in, s, Target::Defaults);
    QCOMPARE(lines.size(), 1);
    QVERIFY(lines.first().reason.contains(QLatin1String("XCL_RAYTRACING=off")));
    QCOMPARE(apply(in, s, Target::Game, kTitle), 1);
    qunsetenv("XCL_RAYTRACING");
    QCOMPARE(apply(in, s, Target::Defaults), 1);
}

void TestSettingsProfile::runProfileTravelsWithoutPaths()
{
    QTemporaryDir dir;
    AppSettings s(dir.filePath(QStringLiteral("a.ini")));
    s.setRunProfile({QStringLiteral("XCL_LOOP_GUARD=2048"), QStringLiteral("XCL_SKIP_SHADERS_FILE=C:\\Users\\alice\\skip.txt"),
                     QStringLiteral("XCL_TRACE_DIR=/home/alice/trace"), QStringLiteral("XCL_SHARE=\\\\nas\\xcl"), QStringLiteral("XCL_HOME=~/xcl"),
                     QStringLiteral("XCL_RATIO=1/2")});
    const Profile p = fromDefaults(s);
    QCOMPARE(p.values.value(QStringLiteral("run_profile")).toStringList(), (QStringList{QStringLiteral("XCL_LOOP_GUARD=2048"), QStringLiteral("XCL_RATIO=1/2")}));
    for (const QByteArray& out : {toJson(p), decodeCode(toCode(p))})
        QVERIFY2(!out.contains("alice") && !out.contains("nas") && !out.contains("~/xcl"), out.constData());

    // the import keeps this PC's own path lines, and the preview shows the profile like any other setting
    AppSettings to(dir.filePath(QStringLiteral("b.ini")));
    to.setRunProfile({QStringLiteral("XCL_LOOP_GUARD=1024"), QStringLiteral("XCL_SKIP_SHADERS_FILE=D:\\mine\\skip.txt")});
    const Parsed in = parse(toCode(p).toUtf8());
    QVERIFY2(in.ok(), qPrintable(in.error));
    QVERIFY(in.rejected.isEmpty());
    const QList<Line> lines = preview(in, to, Target::Defaults);
    const auto profileLine = std::find_if(lines.cbegin(), lines.cend(), [](const Line& l) { return l.key == QLatin1String("run_profile"); });
    QVERIFY(profileLine != lines.cend());
    QCOMPARE(int(profileLine->kind), int(Line::Kind::Change));
    QCOMPARE(profileLine->label, QStringLiteral("Run profile"));
    QCOMPARE(profileLine->from, QStringLiteral("2 variables"));
    QCOMPARE(profileLine->to, QStringLiteral("3 variables (changes XCL_LOOP_GUARD, adds XCL_RATIO)"));
    QCOMPARE(apply(in, to, Target::Defaults), 1);
    QCOMPARE(to.runProfile(), (QStringList{QStringLiteral("XCL_LOOP_GUARD=2048"), QStringLiteral("XCL_RATIO=1/2"), QStringLiteral("XCL_SKIP_SHADERS_FILE=D:\\mine\\skip.txt")}));
    for (const Line& l : preview(in, to, Target::Game, kTitle)) {
        if (l.key == QLatin1String("run_profile"))
            QCOMPARE(int(l.kind), int(Line::Kind::NotApplied));
    }

    // a path that arrives anyway is left out and listed; a malformed profile is refused whole
    const Parsed planted = parse(profileJson({{QStringLiteral("run_profile"), QJsonArray{QStringLiteral("XCL_A=1"), QStringLiteral("XCL_DUMP=C:\\Users\\bob\\d")}}}));
    QCOMPARE(planted.profile.values.value(QStringLiteral("run_profile")).toStringList(), QStringList{QStringLiteral("XCL_A=1")});
    QCOMPARE(planted.rejected.size(), 1);
    QCOMPARE(planted.rejected.first().value, QStringLiteral("XCL_DUMP"));
    for (const QJsonArray& bad : {QJsonArray{QStringLiteral("xcl_a=1")}, QJsonArray{QStringLiteral("XCL_A=1"), QStringLiteral("XCL_A=2")}, QJsonArray{1}}) {
        const Parsed refused = parse(profileJson({{QStringLiteral("run_profile"), bad}}));
        QVERIFY(!refused.profile.values.contains(QStringLiteral("run_profile")));
        QCOMPARE(refused.rejected.size(), 1);
    }
    QCOMPARE(valueLabel(QStringLiteral("run_profile"), AppSettings::defaultRunProfile()), QStringLiteral("Defaults, 20 variables"));
}

QTEST_GUILESS_MAIN(TestSettingsProfile)
#include "tst_settingsprofile.moc"
