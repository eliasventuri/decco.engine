const { app, Tray, Menu } = require('electron');
const path = require('path');
const express = require('express');
const torrentStream = require('torrent-stream');
const cors = require('cors');
const fs = require('fs');
const ffmpeg = require('fluent-ffmpeg');

const PORT = 8888;
const DOWNLOAD_PATH = path.join(app.getPath('userData'), 'downloads');
const SEGMENT_DURATION = 10;
const logFile = fs.createWriteStream(path.join(DOWNLOAD_PATH, 'decco-engine.log'), { flags: 'a' });
const CACHE_META_PATH = path.join(DOWNLOAD_PATH, 'cache-meta.json');
const CACHE_MAX_AGE_MS = 72 * 60 * 60 * 1000; // 72 hours in milliseconds

let tray = null;
const activeEngines = new Map();

const TRACKERS = [
    'udp://opentor.net:6969',
    'http://retracker.local/announce',
    'udp://tracker.opentrackr.org:1337/announce',
    'udp://open.stealth.si:80/announce',
    'http://open.tracker.cl:1337/announce',
    'udp://tracker.torrent.eu.org:451/announce',
    'udp://zer0day.ch:1337/announce',
    'udp://wepzone.net:6969/announce',
    'udp://tracker.srv00.com:6969/announce',
    'udp://tracker.filemail.com:6969/announce',
    'udp://tracker.dler.org:6969/announce',
    'udp://tracker.bittor.pw:1337/announce',
    'udp://tracker-udp.gbitt.info:80/announce',
    'udp://run.publictracker.xyz:6969/announce',
    'udp://opentracker.io:6969/announce',
    'udp://open.dstud.io:6969/announce',
    'udp://explodie.org:6969/announce',
    'https://tracker.iperson.xyz:443/announce',
    'https://torrent.tracker.durukanbal.com:443/announce',
    'https://cny.fan:443/announce',
    'http://tracker2.dler.org:80/announce',
    'http://tracker.wepzone.net:6969/announce',
    'http://bt.t-ru.org/ann?magnet',
    'http://bt2.t-ru.org/ann?magnet',
    'http://bt3.t-ru.org/ann?magnet',
    'http://bt4.t-ru.org/ann?magnet'
];

if (!fs.existsSync(DOWNLOAD_PATH)) fs.mkdirSync(DOWNLOAD_PATH, { recursive: true });

// --- CACHE METADATA MANAGEMENT ---

function loadCacheMeta() {
    try {
        if (fs.existsSync(CACHE_META_PATH)) {
            return JSON.parse(fs.readFileSync(CACHE_META_PATH, 'utf-8'));
        }
    } catch (e) {
        console.log('[Cache] Error loading meta:', e.message);
    }
    return { torrents: {} };
}

function saveCacheMeta(meta) {
    try {
        fs.writeFileSync(CACHE_META_PATH, JSON.stringify(meta, null, 2));
    } catch (e) {
        console.log('[Cache] Error saving meta:', e.message);
    }
}

function addTorrentToCache(hash, info = {}) {
    const meta = loadCacheMeta();
    meta.torrents[hash] = {
        addedAt: Date.now(),
        lastAccessed: Date.now(),
        fileIdx: info.fileIdx || null,
        season: info.season || null,
        episode: info.episode || null
    };
    saveCacheMeta(meta);
    console.log(`[Cache] Added torrent ${hash} to cache meta`);
}

function updateTorrentAccess(hash) {
    const meta = loadCacheMeta();
    if (meta.torrents[hash]) {
        meta.torrents[hash].lastAccessed = Date.now();
        saveCacheMeta(meta);
    }
}

function removeTorrentFromCache(hash) {
    const meta = loadCacheMeta();
    delete meta.torrents[hash];
    saveCacheMeta(meta);
    console.log(`[Cache] Removed torrent ${hash} from cache meta`);
}

// --- SEEDING RESTORATION (on startup) ---

function restoreCachedTorrents() {
    const meta = loadCacheMeta();
    const hashes = Object.keys(meta.torrents);
    console.log(`[Seeding] Restoring ${hashes.length} cached torrents for seeding...`);

    hashes.forEach(hash => {
        const info = meta.torrents[hash];
        // Only restore if not already active
        if (!activeEngines.has(hash)) {
            getEngine(hash, info.fileIdx, info.season, info.episode);
            console.log(`[Seeding] Restored seeder for ${hash}`);
        }
    });
}

// --- CACHE CLEANUP (72h auto-delete) ---

function cleanupOldCache() {
    const meta = loadCacheMeta();
    const now = Date.now();
    let cleaned = 0;

    Object.keys(meta.torrents).forEach(hash => {
        const torrent = meta.torrents[hash];
        const age = now - torrent.lastAccessed;

        if (age > CACHE_MAX_AGE_MS) {
            // Destroy engine if active
            if (activeEngines.has(hash)) {
                try {
                    activeEngines.get(hash).destroy();
                    activeEngines.delete(hash);
                } catch (e) { }
            }

            // Remove from meta
            delete meta.torrents[hash];
            cleaned++;
            console.log(`[Cache] Cleaned old torrent: ${hash} (age: ${Math.round(age / 3600000)}h)`);
        }
    });

    if (cleaned > 0) {
        saveCacheMeta(meta);
        // Also try to clean orphaned files in download directory
        cleanOrphanedFiles(meta);
    }

    console.log(`[Cache] Cleanup complete. Removed ${cleaned} old torrents.`);
}

function cleanOrphanedFiles(meta) {
    try {
        const dirs = fs.readdirSync(DOWNLOAD_PATH);
        dirs.forEach(dir => {
            const dirPath = path.join(DOWNLOAD_PATH, dir);
            if (fs.statSync(dirPath).isDirectory()) {
                // Check if this directory belongs to any active torrent
                const isActive = Array.from(activeEngines.values()).some(engine =>
                    engine.path && engine.path.includes(dir)
                );
                const inMeta = Object.keys(meta.torrents).some(hash => hash.includes(dir.substring(0, 20)));

                if (!isActive && !inMeta && dir !== 'cache-meta.json') {
                    // Safe to delete - orphaned directory
                    fs.rmSync(dirPath, { recursive: true, force: true });
                    console.log(`[Cache] Deleted orphaned directory: ${dir}`);
                }
            }
        });
    } catch (e) {
        console.log('[Cache] Error cleaning orphaned files:', e.message);
    }
}

// --- CLEAR ALL CACHE ---

function clearAllCache() {
    console.log('[Cache] Clearing all cache...');

    // Destroy all engines
    activeEngines.forEach((engine, hash) => {
        try {
            engine.destroy();
        } catch (e) { }
    });
    activeEngines.clear();

    // Clear meta file
    saveCacheMeta({ torrents: {} });

    // Delete all files in download directory (except meta)
    try {
        const items = fs.readdirSync(DOWNLOAD_PATH);
        items.forEach(item => {
            if (item !== 'cache-meta.json') {
                const itemPath = path.join(DOWNLOAD_PATH, item);
                fs.rmSync(itemPath, { recursive: true, force: true });
            }
        });
        console.log('[Cache] All cache cleared successfully');
    } catch (e) {
        console.log('[Cache] Error clearing cache:', e.message);
    }

    // Update tray tooltip
    if (tray) {
        tray.setToolTip('Decco Engine - Cache cleared!');
        setTimeout(() => tray.setToolTip('Decco Engine'), 3000);
    }
}

// Start cleanup interval (every hour)
function startCacheCleanup() {
    cleanupOldCache(); // Run once on startup
    setInterval(cleanupOldCache, 60 * 60 * 1000); // Then every hour
}

// --- EPISODE PATTERN HELPERS ---

// Build regex to match episode patterns like S05E06, S5E6, 5x06, s05.e06, etc.
function buildEpisodePattern(season, episode) {
    const s = String(season).padStart(2, '0');
    const e = String(episode).padStart(2, '0');
    const sNum = String(season);
    const eNum = String(episode);
    // Match: S05E06, S5E6, s05e06, 5x06, 5x6, s05.e06, S05 E06, etc.
    return new RegExp(
        `(s0?${sNum}[.\\s_-]?e0?${eNum}\\b)|(\\b0?${sNum}x0?${eNum}\\b)`,
        'i'
    );
}

// Find the file matching the episode pattern
function findEpisodeFile(files, season, episode) {
    const pattern = buildEpisodePattern(season, episode);
    // First try to find video files matching the pattern
    const videoExtensions = /\.(mkv|mp4|avi|webm|ts|mov|wmv|flv|m4v|3gp|mpg|mpeg|ogv)$/i;
    const matchingVideos = files.filter(f => videoExtensions.test(f.name) && pattern.test(f.name));

    if (matchingVideos.length > 0) {
        // If multiple matches, return the largest one
        return matchingVideos.reduce((a, b) => b.length > a.length ? b : a);
    }

    return null;
}

// --- ENGINE MANAGEMENT ---

function getEngine(hash, fileIdx = null, season = null, episode = null) {
    if (activeEngines.has(hash)) {
        const existingEngine = activeEngines.get(hash);
        // Update file selection if season/episode provided and different
        if (season !== null && episode !== null && existingEngine.metadataReady) {
            const targetPattern = buildEpisodePattern(season, episode);
            const currentFile = existingEngine.videoFile;
            if (currentFile && !targetPattern.test(currentFile.name)) {
                // Current file doesn't match - find correct one
                const correctFile = findEpisodeFile(existingEngine.files, season, episode);
                if (correctFile) {
                    // Deselect all files first
                    existingEngine.files.forEach(f => f.deselect());
                    existingEngine.videoFile = correctFile;
                    correctFile.select();
                    console.log(`[Engine] Corrected file selection to: ${correctFile.name} - Other files deselected`);
                }
            }
        }
        return existingEngine;
    }

    console.log(`[Engine] Creating for hash: ${hash}, fileIdx: ${fileIdx}, S${season}E${episode}`);
    const engine = torrentStream(`magnet:?xt=urn:btih:${hash}&tr=${TRACKERS.map(encodeURIComponent).join('&tr=')}`, {
        tmp: DOWNLOAD_PATH,
        trackers: TRACKERS,
        connections: 100
    });

    engine.status = 'loading';
    engine.metadataReady = false;
    engine.duration = 0;
    engine.isProbing = false;
    engine.requestedFileIdx = fileIdx;
    engine.requestedSeason = season;
    engine.requestedEpisode = episode;

    engine.on('ready', () => {
        let file = null;

        // Priority 1: Search by episode pattern (most reliable)
        if (engine.requestedSeason !== null && engine.requestedEpisode !== null) {
            file = findEpisodeFile(engine.files, engine.requestedSeason, engine.requestedEpisode);
            if (file) {
                console.log(`[Engine] Found file by episode pattern S${engine.requestedSeason}E${engine.requestedEpisode}: ${file.name}`);
            }
        }

        // Priority 2: Use fileIdx if pattern search failed
        if (!file && engine.requestedFileIdx !== null && engine.files[engine.requestedFileIdx]) {
            file = engine.files[engine.requestedFileIdx];
            console.log(`[Engine] Using fileIdx ${engine.requestedFileIdx}: ${file.name}`);
        }

        // Priority 3: Fallback to largest video file
        if (!file) {
            const videoFiles = engine.files.filter(f => f.name.match(/\.(mkv|mp4|avi|webm|ts|mov|wmv|flv|m4v|3gp|mpg|mpeg|ogv)$/i));
            if (videoFiles.length > 0) {
                file = videoFiles.reduce((a, b) => b.length > a.length ? b : a);
            } else {
                file = engine.files[0];
            }
            console.log(`[Engine] Fallback to largest video: ${file.name}`);
        }

        engine.videoFile = file;
        engine.metadataReady = true;
        engine.status = 'ready';

        // IMPORTANT: Deselect ALL files first to prevent downloading entire pack
        engine.files.forEach(f => f.deselect());
        // Then select ONLY the file we need
        file.select();
        console.log(`[Engine] Selected ONLY: ${file.name} (${(file.length / 1024 / 1024).toFixed(1)} MB) - Other ${engine.files.length - 1} files deselected`);

        // Background Duration Probe (via Proxy for speed)
        const probe = () => {
            if (engine.isProbing) return;
            engine.isProbing = true;
            // Probe using the local proxy to test it simultaneously
            ffmpeg.ffprobe(`http://127.0.0.1:${PORT}/proxy/${hash}`, (err, metadata) => {
                engine.isProbing = false;
                if (!err && metadata.format && metadata.format.duration) {
                    engine.duration = metadata.format.duration;

                    // Detect Video Codec
                    const videoStream = metadata.streams.find(s => s.codec_type === 'video');
                    if (videoStream) {
                        engine.videoCodec = videoStream.codec_name; // e.g., 'h264', 'hevc'
                        console.log(`[Engine] Codec Detected: ${engine.videoCodec}`);
                    }

                    console.log(`[Engine] Precise duration found: ${engine.duration}s`);
                } else if (engine.status === 'ready') {
                    // Retry
                    setTimeout(probe, 5000);
                }
            });
        };
        probe();
    });

    engine.on('error', (err) => {
        engine.status = 'error';
        engine.error = err.message;
    });

    activeEngines.set(hash, engine);

    // Register in cache for seeding persistence
    addTorrentToCache(hash, { fileIdx, season, episode });

    return engine;
}

function handleLink(url) {
    const hashMatch = url.match(/decco:\/\/([a-fA-F0-9]+)/);
    const fileIdxMatch = url.match(/fileIdx=(\d+)/);
    const seasonMatch = url.match(/season=(\d+)/);
    const episodeMatch = url.match(/episode=(\d+)/);
    // Also try to extract from path pattern: /tv/...-s05-e06-ID/ or /tv/...-ID-s05-e06/
    const pathSeasonMatch = url.match(/-s(\d+)/i);
    const pathEpisodeMatch = url.match(/-e(\d+)/i);

    if (hashMatch) {
        const hash = hashMatch[1];
        const fileIdx = fileIdxMatch ? parseInt(fileIdxMatch[1], 10) : null;
        const season = seasonMatch ? parseInt(seasonMatch[1], 10) : (pathSeasonMatch ? parseInt(pathSeasonMatch[1], 10) : null);
        const episode = episodeMatch ? parseInt(episodeMatch[1], 10) : (pathEpisodeMatch ? parseInt(pathEpisodeMatch[1], 10) : null);
        console.log(`[Protocol] Handling link - hash: ${hash}, fileIdx: ${fileIdx}, S${season}E${episode}`);
        getEngine(hash, fileIdx, season, episode);
    }
}

// --- APP LIFECYCLE ---

app.setAsDefaultProtocolClient('decco');
const gotLock = app.requestSingleInstanceLock();
if (!gotLock) app.quit();
else {
    app.on('second-instance', (e, cmd) => {
        const url = cmd.find(a => a.startsWith('decco://'));
        if (url) handleLink(url);
    });

    app.on('ready', () => {
        try {
            // Try PNG first (works on all platforms), then ICO
            let iconPath = path.join(__dirname, 'icon.png');
            if (!fs.existsSync(iconPath)) {
                iconPath = path.join(__dirname, 'icon.ico');
            }
            if (fs.existsSync(iconPath)) {
                tray = new Tray(iconPath);
                const contextMenu = Menu.buildFromTemplate([
                    { label: 'Decco Engine: running', enabled: false },
                    { type: 'separator' },
                    { label: 'Clear Cache', click: () => clearAllCache() },
                    { label: 'Restart', click: () => { app.relaunch(); app.exit(0); } },
                    { type: 'separator' },
                    { label: 'Quit Engine', click: () => app.quit() }
                ]);
                tray.setToolTip('Decco Engine');
                tray.setContextMenu(contextMenu);
            } else {
                console.log('[Tray] No icon file found, creating tray without icon');
            }
        } catch (e) { console.log('[Tray] Error creating tray:', e.message); }
        if (process.platform === 'darwin') app.dock.hide();
        try { serverApp.listen(PORT, "127.0.0.1"); } catch (e) { }

        // Start seeding restoration and cache cleanup
        setTimeout(() => {
            restoreCachedTorrents();
            startCacheCleanup();
        }, 3000); // Delay to ensure server is ready

        const url = process.argv.find(a => a.startsWith('decco://'));
        if (url) handleLink(url);
    });
}

// --- SERVER SETUP ---

const serverApp = express();
serverApp.use(cors());

// INTERNAL HTTP PROXY (The "Smart" Layer)
// This translates FFmpeg's Rage requests into Torrent byte reads
serverApp.get('/proxy/:hash', (req, res) => {
    const { hash } = req.params;
    const engine = activeEngines.get(hash);
    if (!engine || !engine.videoFile) return res.status(404).end();

    // Update last accessed time for cache cleanup
    updateTorrentAccess(hash);

    const file = engine.videoFile;
    const range = req.headers.range;

    if (!range) {
        // Fallback for non-ranged requests (metadata probing)
        const stream = file.createReadStream();
        res.writeHead(200, {
            'Content-Length': file.length,
            'Content-Type': 'video/mp4'
        });
        stream.pipe(res);
        return;
    }

    const positions = range.replace(/bytes=/, "").split("-");
    const start = parseInt(positions[0], 10);
    const end = positions[1] ? parseInt(positions[1], 10) : file.length - 1;
    const chunksize = (end - start) + 1;

    console.log(`[Proxy] ${hash}: Serving bytes ${start}-${end} (${chunksize})`);

    res.writeHead(206, {
        "Content-Range": "bytes " + start + "-" + end + "/" + file.length,
        "Accept-Ranges": "bytes",
        "Content-Length": chunksize,
        "Content-Type": "video/mp4"
    });

    const stream = file.createReadStream({ start, end });
    stream.pipe(res);

    // Ensure stream is destroyed if request is aborted (crucial for FFmpeg seeking)
    req.on('close', () => stream.destroy());
});

// HTTP Trigger to start engine (Bypasses custom protocol issues in Dev)
serverApp.get('/start/:hash', (req, res) => {
    const hash = req.params.hash;
    const fileIdx = req.query.fileIdx !== undefined ? parseInt(req.query.fileIdx, 10) : null;
    const season = req.query.season !== undefined ? parseInt(req.query.season, 10) : null;
    const episode = req.query.episode !== undefined ? parseInt(req.query.episode, 10) : null;
    console.log(`[HTTP Start] hash: ${hash}, fileIdx: ${fileIdx}, S${season}E${episode}`);
    getEngine(hash, fileIdx, season, episode);
    res.json({ status: 'started', hash, fileIdx, season, episode });
});

serverApp.get('/status/:hash', (req, res) => {
    const hash = req.params.hash;
    const engine = activeEngines.get(hash);
    if (!engine) return res.json({ status: 'not_started' });
    res.json({
        status: engine.status,
        metadataReady: engine.metadataReady,
        fileName: engine.videoFile ? engine.videoFile.name : null,
        filePath: engine.videoFile ? engine.videoFile.path : null,
        fileSize: engine.videoFile ? engine.videoFile.length : 0,
        fileIdx: engine.requestedFileIdx,
        totalFiles: engine.files ? engine.files.length : 0,
        duration: engine.duration || 0,
        peers: engine.swarm ? engine.swarm.wires.length : 0,
        speed: engine.swarm ? (engine.swarm.downloadSpeed() / 1024).toFixed(2) : '0',
    });
});

// HLS Manifest (Virtual VOD)
serverApp.get('/hls/:hash/index.m3u8', async (req, res) => {
    const hash = req.params.hash;
    const engine = activeEngines.get(hash) || getEngine(hash);

    if (!engine.metadataReady) return res.status(503).send('Wait for metadata');

    const duration = engine.duration > 0 ? engine.duration : 7200;
    let manifest = [
        '#EXTM3U',
        '#EXT-X-VERSION:3',
        `#EXT-X-TARGETDURATION:${SEGMENT_DURATION}`,
        '#EXT-X-MEDIA-SEQUENCE:0',
        '#EXT-X-PLAYLIST-TYPE:VOD'
    ];

    const segmentCount = Math.floor(duration / SEGMENT_DURATION);
    for (let i = 0; i < segmentCount; i++) {
        manifest.push(`#EXTINF:${SEGMENT_DURATION.toFixed(1)},`);
        manifest.push(`segment-${i}.ts`);
    }

    manifest.push('#EXT-X-ENDLIST');

    res.setHeader('Content-Type', 'application/vnd.apple.mpegurl');
    res.send(manifest.join('\n'));
});

// HLS Segment Transcoder (Via Proxy)
serverApp.get('/hls/:hash/segment-:index.ts', (req, res) => {
    const { hash, index } = req.params;
    const engine = activeEngines.get(hash);
    if (!engine || !engine.videoFile) return res.status(404).end();

    const startTime = parseInt(index) * SEGMENT_DURATION;

    // Smart Transcode Logic
    const isH264 = engine.videoCodec === 'h264';
    const transcodeMode = isH264 ? 'Direct Remux' : 'Transcoding (Compatibility)';

    console.log(`[HLS] Parsing seg ${index} (${startTime}s) | Codec: ${engine.videoCodec || 'Unknown'} | Mode: ${transcodeMode}`);

    res.setHeader('Content-Type', 'video/mp2t');

    // Build FFmpeg Command w/ Smart Options
    const ffmpegCommand = ffmpeg(`http://127.0.0.1:${PORT}/proxy/${hash}`)
        .inputOptions([
            `-ss ${startTime}`
        ]);

    const outputOptions = [
        `-t ${SEGMENT_DURATION}`,
        `-output_ts_offset ${startTime}`, // CRITICAL: Fix timestamps for HLS continuity during transcode
        '-c:a aac',       // Always normalize audio to AAC
        '-ac 2',          // Force Stereo
        '-sn',            // Drop subtitles
        '-f mpegts'
    ];

    if (isH264) {
        // H.264 = Direct Copy (Fastest)
        outputOptions.push('-c:v copy');
    } else {
        // HEVC/Other = Transcode (Compatible)
        outputOptions.push('-c:v libx264');
        outputOptions.push('-preset superfast'); // Fast encoding
        outputOptions.push('-crf 23');           // Reasonalble quality
        outputOptions.push('-g 48');             // Keyframe interval for HLS
    }

    ffmpegCommand
        .outputOptions(outputOptions)
        .on('start', (cmd) => console.log(`[FFmpeg] Started: ${cmd}`))
        .on('stderr', (line) => {
            // Only log errors or warnings to avoid clutter
            if (line.includes('Error') || line.includes('warn') || line.includes('fail')) {
                console.log(`[FFmpeg-Log] ${line}`);
            }
        })
        .on('error', (err) => {
            if (!err.message.includes('SIGKILL') && !err.message.includes('404')) {
                console.error(`[HLS-FFmpeg] Critical Error:`, err.message);
            }
        })
        .pipe(res, { end: true });
});

// --- SUBTITLE ENDPOINTS ---

// List embedded subtitle tracks using FFprobe
serverApp.get('/subtitles/:hash', async (req, res) => {
    const { hash } = req.params;
    const engine = activeEngines.get(hash);

    if (!engine || !engine.videoFile) {
        return res.status(404).json({ error: 'Engine or video file not found' });
    }

    const proxyUrl = `http://127.0.0.1:${PORT}/proxy/${hash}`;

    ffmpeg.ffprobe(proxyUrl, (err, metadata) => {
        if (err) {
            console.error('[Subtitles] FFprobe error:', err.message);
            return res.status(500).json({ error: 'Failed to probe video' });
        }

        const subtitleStreams = metadata.streams
            .filter(s => s.codec_type === 'subtitle')
            .map((s, idx) => ({
                index: s.index,
                trackIndex: idx,
                codec: s.codec_name,
                language: s.tags?.language || 'unknown',
                title: s.tags?.title || `Track ${idx + 1}`,
                isForced: s.disposition?.forced === 1,
                isDefault: s.disposition?.default === 1
            }));

        console.log(`[Subtitles] Found ${subtitleStreams.length} subtitle tracks for ${hash}`);
        res.json({ subtitles: subtitleStreams });
    });
});

// Extract subtitle track to VTT format
serverApp.get('/subtitles/:hash/extract/:index', (req, res) => {
    const { hash, index } = req.params;
    const engine = activeEngines.get(hash);

    if (!engine || !engine.videoFile) {
        return res.status(404).json({ error: 'Engine or video file not found' });
    }

    const trackIndex = parseInt(index, 10);
    if (isNaN(trackIndex)) {
        return res.status(400).json({ error: 'Invalid track index' });
    }

    const proxyUrl = `http://127.0.0.1:${PORT}/proxy/${hash}`;

    res.setHeader('Content-Type', 'text/vtt');
    res.setHeader('Access-Control-Allow-Origin', '*');

    console.log(`[Subtitles] Extracting track ${trackIndex} from ${hash}`);

    ffmpeg(proxyUrl)
        .outputOptions([
            `-map 0:s:${trackIndex}`,
            '-c:s webvtt'
        ])
        .format('webvtt')
        .on('error', (err) => {
            if (!err.message.includes('SIGKILL')) {
                console.error('[Subtitles] Extraction error:', err.message);
            }
        })
        .pipe(res, { end: true });
});
