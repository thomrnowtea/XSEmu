#pragma once

#include "AppSettings.h"

#include <QDateTime>
#include <QSet>
#include <QFileInfo>
#include <QString>
#include <QStringList>

#include <atomic>

// What the launcher can truthfully say about the shader cache, read from the files the emulator
// writes (crates/xcl/src/hle/d3d12x/pso_manifest.rs and real.rs). The cache is machine-wide, not
// per title: <dxil>\pipelines\pipelines.bin lists every pipeline any title made (u32 length + entry,
// keyed by FNV-1a 64 of the entry | 1), <dxil>\pipelines\runs counts the game runs, and the driver
// blobs the warm-up built are <dxil>\driver\<vendor-device-umd>\*.pso. A value of -1 means Unknown.
struct ShaderCacheInfo {
    QString dxilDir;
    bool scanned = false;          // read from disk; a default value says nothing about the cache
    bool exists = false;           // the DXIL cache folder exists
    qint64 pipelines = -1;         // distinct pipelines recorded (-1: no pipelines.bin)
    qint64 manifestEntries = -1;   // entries in pipelines.bin, repeats included
    qint64 manifestBytes = -1;
    int runs = -1;                 // game runs counted (-1: no runs file)
    qint64 blobs = -1;             // *.pso driver blobs, all adapters (-1: no driver folder)
    qint64 blobBytes = -1;
    int adapters = 0;              // driver blob folders (one per GPU + driver version)
    QDateTime newestBlob;          // when the newest blob was written (invalid: none)
    qint64 totalBytes = -1;        // every file in the cache folder

    // What has been parsed of pipelines.bin so far, so a later read parses only the appended tail.
    // The emulator also rewrites the file (Store::rewrite drops broken and repeated entries from the
    // middle, through a temporary file and a rename) and may append past the old end before the next
    // read. So a read resumes only when the path, the file's creation time and its first bytes are
    // unchanged, it has not shrunk, and the last entry parsed is still at its offset with the same
    // length and bytes; otherwise it parses from the start.
    struct Manifest {
        QString path;
        QDateTime born;
        QByteArray head;
        qint64 offset = 0;      // the end of the last complete entry
        qint64 entries = 0;
        QSet<quint64> keys;
        qint64 lastStart = -1;  // the last complete entry: where its length prefix starts,
        quint32 lastLen = 0;    // its length
        quint64 lastHash = 0;   // and fnv64 of its bytes
        qint64 badAt = -1;      // where a length prefix longer than any entry stopped the last read
    };
    // An entry is one encoded pipeline record; its shaders live in their own files, so a real entry
    // is a few KB. A longer length prefix means the parse is not on an entry boundary.
    static constexpr quint32 kMaxManifestEntry = 16u << 20;
    // Parses the new part of the manifest at `path` in chunks; false when the file is missing.
    static bool readManifest(const QString& path, Manifest& m, const std::atomic<bool>* cancel = nullptr);

    // Walks the whole folder; slow on a real cache, so only ever called off the UI thread.
    static ShaderCacheInfo read(const QString& dxilDir);
    static ShaderCacheInfo read(const QString& dxilDir, Manifest& manifest, const std::atomic<bool>* cancel = nullptr);

    // FNV-1a 64 as the emulator computes it (gpu::shader::fnv64).
    static quint64 fnv64(const char* data, qsizetype len);

    // An entry at the top of the cache folder that the emulator writes (crates/xcl): the pipelines,
    // driver and translated folders (pso_manifest.rs, real.rs, gpu/recomp/pso.rs), <16 hex>.dxil
    // and its <16 hex>.tmp<pid> partial (gpu/recomp/dxc.rs), used-pipelines.txt and
    // skipped-pipelines.txt (pso_async.rs). Clear deletes these and nothing else.
    static bool isCacheEntry(const QFileInfo& entry);

    struct Cleared {
        bool ok = true;      // every cache entry was deleted
        QStringList kept;    // other entries in the folder, left alone
        QString error;       // why it refused or what could not be deleted
    };
    // Delete the emulator's entries in the cache folder and keep the folder and everything else.
    // Refuses a drive root and the home folder.
    static Cleared clear(const QString& dxilDir);
};

// A game's warm-up state as far as the launcher can tell: from its own warm-up records (the
// emulator keeps no per-title shader state, so a game nobody warmed up from this launcher is Unknown
// unless the machine-wide cache is empty). A finished warm-up is Ready only while the cache still
// holds driver blobs, so a cleared cache reads as NeverWarmed, and only while it ran with the game's
// current run profile (profile: LaunchConfig::profileIdentity; a record or a profile that is empty is
// not compared), so a changed translator switch reads as ProfileChanged. Before the cache is scanned
// nothing is claimed about it: only the launcher's own records count.
enum class TitleShaderState { Unknown, NeverWarmed, Ready, LastRunIncomplete, LastRunFailed, ProfileChanged };

TitleShaderState titleShaderState(const QString& titleId, const QList<WarmupRun>& recent, const ShaderCacheInfo& cache, const QString& profile = {});
// The state the launcher shows and acts on: titleShaderState with the game's current run profile, or
// Unknown while the XSEmu warm-up is turned off (AppSettings::xsemuWarmup), so nothing asks for it.
TitleShaderState shownShaderState(const AppSettings& settings, const QString& titleId, const QList<WarmupRun>& recent, const ShaderCacheInfo& cache);
