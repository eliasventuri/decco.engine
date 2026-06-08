const { app, Tray, Menu, BrowserWindow, shell } = require('electron');
const path = require('path');
const express = require('express');
const torrentStream = require('torrent-stream');
const cors = require('cors');
const fs = require('fs');
const ffmpeg = require('fluent-ffmpeg');
const { autoUpdater } = require('electron-updater');
const { Readable } = require('stream');

let PORT = 18889;
const DOWNLOAD_PATH = path.join(app.getPath('userData'), 'downloads');
const SEGMENT_DURATION = 10;
const logFile = fs.createWriteStream(path.join(DOWNLOAD_PATH, 'decco-engine.log'), { flags: 'a' });
const CACHE_META_PATH = path.join(DOWNLOAD_PATH, 'cache-meta.json');
const CACHE_MAX_AGE_MS = 72 * 60 * 60 * 1000; // 72 hours in milliseconds

// --- DOWNLOAD SYSTEM ---
const DOWNLOADS_DIR = path.join(app.getPath('userData'), 'completed-downloads');
const DOWNLOADS_META_PATH = path.join(DOWNLOADS_DIR, 'downloads-meta.json');
if (!fs.existsSync(DOWNLOADS_DIR)) fs.mkdirSync(DOWNLOADS_DIR, { recursive: true });

const activeDownloads = new Map(); // hash -> { engine, meta, interval }
let downloadsWindow = null;

let tray = null;
const activeEngines = new Map();

// --- DOWNLOADS META PERSISTENCE ---

function getDownloadId(hash, season, episode, fileIdx) {
    if (season && episode && (parseInt(season) > 0 || parseInt(episode) > 0)) {
        return `${hash}_s${season}e${episode}`;
    }
    if (fileIdx !== undefined && fileIdx !== null && String(fileIdx) !== 'null') {
        return `${hash}_f${fileIdx}`;
    }
    return hash;
}

function findExactVideoFile(dir, fileName) {
    if (!fs.existsSync(dir)) return null;
    let foundPath = null;

    function scan(currentDir) {
        try {
            const files = fs.readdirSync(currentDir);
            for (const file of files) {
                const fullPath = path.join(currentDir, file);
                const stat = fs.statSync(fullPath);
                if (stat.isDirectory()) {
                    scan(fullPath);
                } else if (file === fileName) {
                    foundPath = fullPath;
                    return;
                }
            }
        } catch (e) {
            console.error(`[findExactVideoFile] Error reading ${currentDir}:`, e.message);
        }
    }

    scan(dir);
    return foundPath || findVideoFile(dir);
}

function loadDownloadsMeta() {
    try {
        if (fs.existsSync(DOWNLOADS_META_PATH)) {
            return JSON.parse(fs.readFileSync(DOWNLOADS_META_PATH, 'utf-8'));
        }
    } catch (e) {
        console.log('[Downloads] Error loading meta:', e.message);
        try {
            const backupPath = DOWNLOADS_META_PATH + '.corrupted.' + Date.now();
            fs.renameSync(DOWNLOADS_META_PATH, backupPath);
            console.log(`[Downloads] Corrupted meta backed up to: ${backupPath}`);
        } catch (err) {
            console.log('[Downloads] Failed to backup corrupted meta:', err.message);
        }
    }
    return { downloads: {} };
}

function saveDownloadsMeta(meta) {
    try {
        fs.writeFileSync(DOWNLOADS_META_PATH, JSON.stringify(meta, null, 2));
    } catch (e) {
        console.log('[Downloads] Error saving meta:', e.message);
    }
}

// --- SRT TO WEBVTT CONVERTER ---

function srtToWebVTT(srtContent) {
    let vtt = 'WEBVTT\n\n';
    // Normalize line endings
    const lines = srtContent.replace(/\r\n/g, '\n').replace(/\r/g, '\n').split('\n');
    for (let i = 0; i < lines.length; i++) {
        const line = lines[i].trim();
        // Convert SRT timestamps: 00:01:23,456 --> 00:01:25,789  to  00:01:23.456 --> 00:01:25.789
        if (line.includes('-->') && line.includes(',')) {
            vtt += line.replace(/,/g, '.') + '\n';
        } else {
            vtt += line + '\n';
        }
    }
    return vtt;
}

async function downloadSubtitles(hash, title, imdbId, season, episode, videoFilePath) {
    if (!imdbId) return [];
    try {
        const url = `https://decco.tv/api/subtitles/external?imdbId=${imdbId}&season=${season || 0}&episode=${episode || 0}`;
        console.log(`[Downloads] Fetching subtitles from: ${url}`);
        const response = await fetch(url);
        if (!response.ok) return [];
        const data = await response.json();
        const subtitles = data.subtitles || data || [];
        if (!Array.isArray(subtitles) || subtitles.length === 0) return [];

        const savedSubs = [];
        for (const sub of subtitles) { // Download all available subtitle tracks
            try {
                const subUrl = sub.url || sub.src;
                if (!subUrl) continue;
                const subRes = await fetch(subUrl);
                if (!subRes.ok) continue;
                let content = await subRes.text();
                const lang = sub.lang || sub.language || 'unknown';
                const label = sub.label || lang;
                
                const baseName = path.parse(videoFilePath).name;
                const videoDir = path.dirname(videoFilePath);
                if (!fs.existsSync(videoDir)) {
                    fs.mkdirSync(videoDir, { recursive: true });
                }
                
                // VLC ONLY reliably detects language track names from filenames if:
                // 1. The extension is a classic subtitle format like .srt
                // 2. The language code is an ISO 639 code (e.g. 'es', 'en')
                // If we use .vtt or full words like 'Spanish', VLC defaults to 'Track 1'.
                const srtPath = path.join(videoDir, `${baseName}.${lang}.srt`);
                fs.writeFileSync(srtPath, content, 'utf-8');
                savedSubs.push({ lang, label: label, path: srtPath });
                console.log(`[Downloads] Saved subtitle: ${srtPath}`);
            } catch (e) {
                console.log(`[Downloads] Failed to download subtitle:`, e.message);
            }
        }
        return savedSubs;
    } catch (e) {
        console.log('[Downloads] Subtitle fetch error:', e.message);
        return [];
    }
}

// --- MULTIPACK CLEANUP ---
// In season-pack/multipack torrents, BitTorrent pieces can span file boundaries.
// Downloading S02E15 may cause S02E14's data to also be written to disk because
// they share boundary pieces. This function removes unwanted video files.

function cleanupUnwantedFiles(downloadDir, wantedFileName) {
    const videoExtensions = /\.(mkv|mp4|avi|webm|ts|mov|wmv|flv|m4v|3gp|mpg|mpeg|ogv)$/i;

    // Build a set of ALL wanted file names across every download entry
    // that shares this same downloadDir (i.e. same torrent hash).
    // This prevents deleting E14 when E15 completes if E14 was also
    // intentionally downloaded by the user.
    const wantedFiles = new Set();
    wantedFiles.add(wantedFileName);
    try {
        const allMeta = loadDownloadsMeta();
        for (const entry of Object.values(allMeta.downloads)) {
            if (entry.downloadDir === downloadDir && entry.fileName) {
                wantedFiles.add(entry.fileName);
            }
        }
    } catch (e) {
        console.log(`[Downloads] Could not read meta for cleanup: ${e.message}`);
    }

    function scanAndClean(dir) {
        try {
            if (!fs.existsSync(dir)) return;
            const entries = fs.readdirSync(dir, { withFileTypes: true });
            for (const entry of entries) {
                const fullPath = path.join(dir, entry.name);
                if (entry.isDirectory()) {
                    scanAndClean(fullPath);
                } else if (videoExtensions.test(entry.name) && !wantedFiles.has(entry.name)) {
                    try {
                        fs.unlinkSync(fullPath);
                        console.log(`[Downloads] Cleaned up unwanted multipack file: ${entry.name}`);
                    } catch (e) {
                        console.log(`[Downloads] Failed to clean up ${entry.name}: ${e.message}`);
                    }
                }
            }
        } catch (e) {
            console.log(`[Downloads] Cleanup scan error in ${dir}: ${e.message}`);
        }
    }
    
    scanAndClean(downloadDir);
}

// --- DOWNLOAD ENGINE MANAGEMENT ---

function getFileProgress(engine, file) {
    if (!engine || !file || !engine.torrent) return 0;
    try {
        const torrent = engine.torrent;
        const pieceLength = torrent.pieceLength || engine.pieceLength;
        if (!pieceLength || pieceLength <= 0) return 0;
        const startPiece = Math.floor(file.offset / pieceLength);
        const endPiece = Math.floor((file.offset + file.length - 1) / pieceLength);
        let have = 0;
        const total = endPiece - startPiece + 1;
        for (let i = startPiece; i <= endPiece; i++) {
            if (engine.bitfield && engine.bitfield.get(i)) have++;
        }
        return total > 0 ? have / total : 0;
    } catch (e) {
        return 0;
    }
}

function startDownload(hash, title, imdbId, season, episode, fileIdx) {
    const downloadId = getDownloadId(hash, season, episode, fileIdx);

    if (activeDownloads.has(downloadId)) {
        console.log(`[Downloads] Already downloading: ${downloadId}`);
        return activeDownloads.get(downloadId).meta;
    }

    const downloadDir = path.join(DOWNLOADS_DIR, hash.substring(0, 16));
    if (!fs.existsSync(downloadDir)) fs.mkdirSync(downloadDir, { recursive: true });

    // Check if there is an active streaming engine for this hash
    const streamingEngine = activeEngines.get(hash);
    let wasStreaming = false;
    if (streamingEngine && !streamingEngine.isMock) {
        console.log(`[Downloads] Found active streaming engine for ${hash}. Upgrading to permanent download...`);
        wasStreaming = true;
        
        // Get the source path of the streaming engine before destroying it
        const srcPath = streamingEngine.path || (streamingEngine.torrent && streamingEngine.torrent.path);
        
        try {
            // Destroy the streaming engine to release file locks on the cache
            streamingEngine.destroy();
            activeEngines.delete(hash);
            console.log(`[Downloads] Active streaming engine destroyed for transition.`);
        } catch (e) {
            console.error(`[Downloads] Error destroying streaming engine for transition:`, e.message);
        }

        // Copy already cached files to the permanent download directory so progress is preserved
        if (srcPath && fs.existsSync(srcPath)) {
            try {
                console.log(`[Downloads] Copying cached files from "${srcPath}" to "${downloadDir}"...`);
                fs.cpSync(srcPath, downloadDir, { recursive: true, force: true, errorOnExist: false });
                console.log(`[Downloads] Copied cached files successfully.`);

                // Clean up the temporary streaming files now that they are copied to completed-downloads
                try {
                    fs.rmSync(srcPath, { recursive: true, force: true });
                    console.log(`[Downloads] Cleaned up temporary stream files from "${srcPath}"`);
                } catch (rmErr) {
                    console.error(`[Downloads] Failed to delete temporary stream files:`, rmErr.message);
                }

                // Also remove the torrent from the streaming cache metadata since it is now a permanent download
                try {
                    removeTorrentFromCache(hash);
                } catch (metaErr) {
                    console.error(`[Downloads] Failed to remove torrent from cache metadata:`, metaErr.message);
                }
            } catch (e) {
                console.error(`[Downloads] Failed to copy cached files:`, e.message);
            }
        }
    }

    console.log(`[Downloads] Starting download: ${title} (${hash}) [ID: ${downloadId}]`);

    const engine = torrentStream(`magnet:?xt=urn:btih:${hash}&tr=${TRACKERS.map(encodeURIComponent).join('&tr=')}`, {
        tmp: downloadDir,
        path: downloadDir,
        trackers: TRACKERS,
        connections: 100
    });

    // If it was streaming, immediately register the new download engine in activeEngines
    // so any incoming playback requests (/proxy, /status) seamlessly route to it.
    if (wasStreaming) {
        engine.isMock = false;
        engine.requestedFileIdx = fileIdx;
        engine.requestedSeason = season;
        engine.requestedEpisode = episode;
        engine.metadataReady = false;
        engine.status = 'loading';
        
        engine.on('ready', () => {
            engine.metadataReady = true;
            engine.status = 'ready';
        });
        
        activeEngines.set(hash, engine);
    }

    const existingMeta = loadDownloadsMeta().downloads[downloadId];
    const isAlreadyCompleted = existingMeta && existingMeta.status === 'completed';

    const meta = {
        id: downloadId,
        hash,
        title: title || 'Unknown',
        imdbId: imdbId || '',
        season: season || 0,
        episode: episode || 0,
        fileIdx: fileIdx,
        status: isAlreadyCompleted ? 'completed' : 'loading',
        progress: isAlreadyCompleted ? 1.0 : 0,
        speed: 0,
        peers: 0,
        fileName: existingMeta ? existingMeta.fileName : null,
        fileSize: existingMeta ? existingMeta.fileSize : 0,
        downloadDir,
        subtitles: existingMeta ? existingMeta.subtitles : [],
        startedAt: existingMeta ? existingMeta.startedAt : Date.now(),
        completedAt: existingMeta ? existingMeta.completedAt : null
    };

    engine.on('ready', () => {
        let file = null;

        // Priority 1: Episode pattern match
        if (season && episode) {
            file = findEpisodeFile(engine.files, season, episode);
        }
        // Priority 2: fileIdx
        if (!file && fileIdx !== undefined && fileIdx !== null && engine.files[fileIdx]) {
            file = engine.files[fileIdx];
        }
        // Priority 3: Largest video file
        if (!file) {
            const videoFiles = engine.files.filter(f => f.name.match(/\.(mkv|mp4|avi|webm|ts|mov|wmv|flv|m4v)$/i));
            file = videoFiles.length > 0
                ? videoFiles.reduce((a, b) => b.length > a.length ? b : a)
                : engine.files[0];
        }

        // Deselect all, select only target
        if (!file) {
            console.error(`[Downloads] No video file found for hash: ${hash}`);
            meta.status = 'error';
            persistDownloadMeta(downloadId, meta);
            return;
        }

        engine.files.forEach(f => f.deselect());
        file.select();

        if (meta.status !== 'completed') {
            meta.status = 'downloading';
        }
        meta.fileName = file.name;
        meta.fileSize = file.length;
        engine.videoFile = file;

        console.log(`[Downloads] Selected file: ${file.name} (${(file.length / 1024 / 1024).toFixed(1)} MB)`);

        // Start subtitle download in background
        const videoFilePath = path.join(downloadDir, file.path);
        downloadSubtitles(hash, title, imdbId, season, episode, videoFilePath)
            .then(subs => {
                meta.subtitles = subs;
                persistDownloadMeta(downloadId, meta);
            });

        // Progress polling
        const interval = setInterval(() => {
            const progress = getFileProgress(engine, file);
            meta.progress = progress;
            meta.speed = engine.swarm ? engine.swarm.downloadSpeed() : 0;
            meta.peers = engine.swarm ? engine.swarm.wires.length : 0;

            if (progress >= 1.0 && meta.status === 'downloading') {
                meta.status = 'completed';
                meta.completedAt = Date.now();
                meta.progress = 1.0;
                clearInterval(interval);
                console.log(`[Downloads] COMPLETED: ${meta.title}`);
                persistDownloadMeta(downloadId, meta);

                // Clean up unwanted video files from multipack/season-pack torrents.
                // BitTorrent pieces can span file boundaries, so downloading one episode
                // may cause adjacent episodes' data to also be written to disk.
                cleanupUnwantedFiles(downloadDir, file.name);
            }
        }, 1000);

        activeDownloads.set(downloadId, { engine, meta, interval, file });
        persistDownloadMeta(downloadId, meta);
    });

    engine.on('error', (err) => {
        meta.status = 'error';
        console.error(`[Downloads] Error for ${hash}:`, err.message);
        persistDownloadMeta(downloadId, meta);
    });

    activeDownloads.set(downloadId, { engine, meta, interval: null });
    return meta;
}

function persistDownloadMeta(downloadId, meta) {
    const allMeta = loadDownloadsMeta();
    allMeta.downloads[downloadId] = {
        id: downloadId,
        hash: meta.hash,
        title: meta.title,
        imdbId: meta.imdbId,
        season: meta.season,
        episode: meta.episode,
        fileIdx: meta.fileIdx,
        fileName: meta.fileName,
        fileSize: meta.fileSize,
        downloadDir: meta.downloadDir,
        subtitles: meta.subtitles,
        status: meta.status,
        startedAt: meta.startedAt,
        completedAt: meta.completedAt
    };
    saveDownloadsMeta(allMeta);
}

function restoreDownloads() {
    const allMeta = loadDownloadsMeta();
    const downloadIds = Object.keys(allMeta.downloads);
    console.log(`[Downloads] Restoring ${downloadIds.length} downloads...`);
    downloadIds.forEach(id => {
        const saved = allMeta.downloads[id];
        if (saved.status === 'downloading' || saved.status === 'loading' || saved.status === 'completed') {
            startDownload(saved.hash, saved.title, saved.imdbId, saved.season, saved.episode, saved.fileIdx);
        }
    });
}

// --- DOWNLOADS WINDOW ---

function createDownloadsWindow() {
    if (downloadsWindow && !downloadsWindow.isDestroyed()) {
        downloadsWindow.show();
        downloadsWindow.focus();
        return downloadsWindow;
    }

    downloadsWindow = new BrowserWindow({
        width: 720,
        height: 580,
        title: 'Decco — Descargas',
        icon: path.join(__dirname, 'icon.png'),
        autoHideMenuBar: true,
        backgroundColor: '#0a0a0a',
        webPreferences: {
            nodeIntegration: false,
            contextIsolation: true
        }
    });

    downloadsWindow.loadFile(path.join(__dirname, 'downloads.html'));

    downloadsWindow.on('closed', () => {
        downloadsWindow = null;
    });

    return downloadsWindow;
}

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

// --- CLEAR ALL STREAMING CACHE (does NOT touch downloads) ---

function clearAllCache() {
    console.log('[Cache] Clearing streaming cache only (downloads are NOT affected)...');

    // Destroy ONLY streaming engines — never touch activeDownloads
    activeEngines.forEach((engine, hash) => {
        try {
            engine.destroy();
        } catch (e) { }
    });
    activeEngines.clear();

    // Clear streaming cache meta
    saveCacheMeta({ torrents: {} });

    // Delete streaming cache files in DOWNLOAD_PATH (NOT DOWNLOADS_DIR)
    // Preserve: cache-meta.json, decco-engine.log
    const protectedFiles = new Set(['cache-meta.json', 'decco-engine.log']);
    try {
        const items = fs.readdirSync(DOWNLOAD_PATH);
        items.forEach(item => {
            if (!protectedFiles.has(item)) {
                const itemPath = path.join(DOWNLOAD_PATH, item);
                fs.rmSync(itemPath, { recursive: true, force: true });
            }
        });
        console.log('[Cache] Streaming cache cleared successfully');
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
        if (season !== null && episode !== null && existingEngine.metadataReady && !existingEngine.isMock) {
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

    // Check downloads database first
    const allMeta = loadDownloadsMeta();
    const downloadId = getDownloadId(hash, season, episode, fileIdx);
    const entry = allMeta.downloads[downloadId];
    if (entry) {
        if (entry.status === 'completed') {
            console.log(`[Engine] ${hash} (ID: ${downloadId}) is fully downloaded. Creating mock engine in getEngine.`);
            const filePath = findExactVideoFile(entry.downloadDir, entry.fileName);
            if (filePath) {
                const fileName = path.basename(filePath);
                const fileSize = fs.statSync(filePath).size;
                
                const mockEngine = {
                    status: 'ready',
                    metadataReady: true,
                    videoFile: {
                        name: fileName,
                        path: filePath,
                        length: fileSize
                    },
                    requestedFileIdx: fileIdx,
                    files: [{ name: fileName, length: fileSize }],
                    duration: entry.duration || 0,
                    isMock: true
                };
                
                activeEngines.set(hash, mockEngine);
                
                // Probe local file in background for duration using the HTTP proxy URL (reliable and platform-agnostic)
                if (!mockEngine.duration) {
                    ffmpeg.ffprobe(`http://127.0.0.1:${PORT}/proxy/${hash}`, (err, metadata) => {
                        if (!err && metadata.format && metadata.format.duration) {
                            mockEngine.duration = metadata.format.duration;
                            console.log(`[Mock Engine] Precise duration found in getEngine: ${mockEngine.duration}s`);
                            
                            // Save duration in persistent downloads metadata
                            try {
                                const freshMeta = loadDownloadsMeta();
                                const savedItem = freshMeta.downloads[downloadId];
                                if (savedItem) {
                                    savedItem.duration = mockEngine.duration;
                                    saveDownloadsMeta(freshMeta);
                                    console.log(`[Mock Engine] Saved precise duration to downloads database for ${downloadId}`);
                                }
                            } catch (saveErr) {
                                console.error('[Mock Engine] Failed to save duration to database:', saveErr.message);
                            }
                        } else if (err) {
                            console.error(`[Mock Engine] Duration probe failed: ${err.message}`);
                        }
                    });
                }
                
                return mockEngine;
            }
        } else {
            // Downloading, loading or paused item - start/resume using download manager context to share progress!
            console.log(`[Engine] Torrent ${hash} is currently downloading/paused in the database. Sharing download context.`);
            let activeDl = activeDownloads.get(downloadId);
            if (!activeDl) {
                // Initialize startDownload to configure torrentStream inside the correct folder
                startDownload(hash, entry.title, entry.imdbId, entry.season, entry.episode, entry.fileIdx);
                activeDl = activeDownloads.get(downloadId);
            }

            if (activeDl && activeDl.engine) {
                const dlEngine = activeDl.engine;
                dlEngine.isMock = false;
                dlEngine.requestedFileIdx = fileIdx;
                dlEngine.requestedSeason = season;
                dlEngine.requestedEpisode = episode;
                
                // Helper to probe duration and codec for the active downloading torrent when played
                const probeDl = () => {
                    if (dlEngine.duration && dlEngine.videoCodec) return; // Already probed
                    if (dlEngine.isProbing) return;
                    dlEngine.isProbing = true;
                    
                    ffmpeg.ffprobe(`http://127.0.0.1:${PORT}/proxy/${hash}`, (err, metadata) => {
                        dlEngine.isProbing = false;
                        if (!err && metadata.format && metadata.format.duration) {
                            dlEngine.duration = metadata.format.duration;
                            
                            const videoStream = metadata.streams.find(s => s.codec_type === 'video');
                            if (videoStream) {
                                dlEngine.videoCodec = videoStream.codec_name;
                                console.log(`[DL Engine] Codec Detected: ${dlEngine.videoCodec}`);
                            }
                            console.log(`[DL Engine] Precise duration found: ${dlEngine.duration}s`);
                            
                            // Save duration in persistent downloads metadata
                            try {
                                const freshMeta = loadDownloadsMeta();
                                const savedItem = freshMeta.downloads[downloadId];
                                if (savedItem) {
                                    savedItem.duration = dlEngine.duration;
                                    saveDownloadsMeta(freshMeta);
                                    console.log(`[DL Engine] Saved precise duration to downloads database for ${downloadId}`);
                                }
                            } catch (saveErr) {
                                console.error('[DL Engine] Failed to save duration to database:', saveErr.message);
                            }
                        } else if (dlEngine.status === 'ready') {
                            setTimeout(probeDl, 5000);
                        }
                    });
                };
                
                if (dlEngine.videoFile) {
                    dlEngine.metadataReady = true;
                    dlEngine.status = 'ready';
                    probeDl();
                } else {
                    dlEngine.on('ready', () => {
                        dlEngine.metadataReady = true;
                        dlEngine.status = 'ready';
                        probeDl();
                    });
                }
                
                activeEngines.set(hash, dlEngine);
                return dlEngine;
            }
        }
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
        if (!file) {
            console.error(`[Engine] No video file found for hash: ${hash}`);
            engine.status = 'error';
            engine.error = 'No video file found';
            return;
        }

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

    const UPDATE_INFO_PATH = path.join(DOWNLOAD_PATH, 'update-info.json');

    function loadUpdateInfo() {
        try {
            if (fs.existsSync(UPDATE_INFO_PATH)) {
                return JSON.parse(fs.readFileSync(UPDATE_INFO_PATH, 'utf-8'));
            }
        } catch (e) { }
        return { lastUpdated: null, version: app.getVersion() };
    }

    function saveUpdateInfo(info) {
        try {
            fs.writeFileSync(UPDATE_INFO_PATH, JSON.stringify(info, null, 2));
        } catch (e) { }
    }

    let updateReady = false;

    function updateTrayMenu() {
        if (!tray) return;

        const updateInfo = loadUpdateInfo();
        const lastUpdated = updateInfo.lastUpdated ? new Date(updateInfo.lastUpdated).toLocaleString() : 'Never';
        const version = app.getVersion();

        const contextMenu = Menu.buildFromTemplate([
            { label: `Decco Engine: v${version}`, enabled: false },
            { label: `Last Updated: ${lastUpdated}`, enabled: false },
            { type: 'separator' },
            {
                label: 'Check for Updates', click: () => {
                    console.log('[Tray] User requested update check');
                    autoUpdater.checkForUpdatesAndNotify();
                    tray.displayBalloon({ title: 'Decco Engine', content: 'Checking for updates...' });
                }
            },
            { type: 'separator' },
            updateReady ? {
                label: 'Install Update and Restart',
                click: () => {
                    console.log('[Tray] Installing update and restarting...');
                    autoUpdater.quitAndInstall(true, true);
                }
            } : { label: 'Restart', click: () => { app.relaunch(); app.exit(0); } },

            { label: 'Manage Downloads', click: () => shell.openExternal('https://decco.tv/downloads') },
            { label: 'Clear Cache', click: () => clearAllCache() },
            { type: 'separator' },
            { label: 'Quit Engine', click: () => app.quit() }
        ]);

        tray.setToolTip(updateReady ? `Decco Engine v${version} (Update Ready)` : `Decco Engine v${version}`);
        tray.setContextMenu(contextMenu);
    }

    // Check if version changed since last run to update timestamp
    function checkVersionChange() {
        const info = loadUpdateInfo();
        const currentVersion = app.getVersion();

        if (info.version !== currentVersion || !info.lastUpdated) {
            info.version = currentVersion;
            info.lastUpdated = Date.now();
            saveUpdateInfo(info);
            console.log(`[Updater] Version changed to ${currentVersion} or first run. Updated timestamp.`);
        }
    }

    app.on('ready', () => {
        try {
            // Check version change first
            checkVersionChange();

            // Auto-start on login (Windows Registry - Direct Write)
            // Electron's setLoginItemSettings can silently fail for per-machine installs,
            // so we write directly to the registry as a reliable fallback.
            if (app.isPackaged && process.platform === 'win32') {
                const exePath = app.getPath('exe');
                const regValue = `"${exePath}" --hidden`;
                const { exec } = require('child_process');
                exec(`reg add "HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Run" /v DeccoEngine /t REG_SZ /d "${regValue}" /f`, (err) => {
                    if (err) {
                        console.log('[AutoStart] Failed to write registry:', err.message);
                    } else {
                        console.log('[AutoStart] Registry entry set successfully:', regValue);
                    }
                });
            }

            // Try PNG first (works on all platforms), then ICO
            let iconPath = path.join(__dirname, 'icon.png');
            if (!fs.existsSync(iconPath)) {
                iconPath = path.join(__dirname, 'icon.ico');
            }
            if (fs.existsSync(iconPath)) {
                tray = new Tray(iconPath);
                updateTrayMenu(); // Use dynamic menu
            } else {
                console.log('[Tray] No icon file found, creating tray without icon');
            }
        } catch (e) { console.log('[Tray] Error creating tray:', e.message); }
        if (process.platform === 'darwin') app.dock.hide();
        
        // Active ports tracking
        const activePorts = [];
        const registerActivePort = (port) => {
            if (!activePorts.includes(port)) {
                activePorts.push(port);
            }
            PORT = activePorts[0];
            console.log(`[Server] Registered active port: ${port}. Primary engine port is now: ${PORT}`);
        };

        // Start the server by binding primarily to "127.0.0.1" (explicit loopback) to prevent Windows Firewall alerts
        const startServerOnPort = (port) => {
            const http = require('http');
            const server = http.createServer(serverApp);

            server.listen(port, "127.0.0.1", () => {
                console.log(`[Server] Express server listening on explicit loopback 127.0.0.1:${port} (Firewall-safe)`);
                registerActivePort(port);
            });

            server.on('error', (err) => {
                console.error(`[Server] Failed to bind to '127.0.0.1' on port ${port}:`, err.message);
                if (err.code === 'EADDRNOTAVAIL' || err.code === 'EINVAL') {
                    console.log(`[Server] Loopback not available. Trying dual-stack "::" on port ${port}...`);
                    const fallbackServer = http.createServer(serverApp);
                    fallbackServer.listen(port, "::", () => {
                        console.log(`[Server] Express server listening on port ${port} (IPv4/IPv6 dual-stack)`);
                        registerActivePort(port);
                    });
                    fallbackServer.on('error', (err2) => {
                        console.error(`[Server] Failed to bind to '::' on port ${port}:`, err2.message);
                        console.log(`[Server] Retrying with "0.0.0.0" on port ${port}...`);
                        const ultimateServer = http.createServer(serverApp);
                        ultimateServer.on('error', (err3) => {
                            console.error(`[Server] Ultimate fallback to 0.0.0.0 failed on port ${port}:`, err3.message);
                        });
                        ultimateServer.listen(port, "0.0.0.0", () => {
                            console.log(`[Server] Express server listening on port ${port} (all IPv4 interfaces)`);
                            registerActivePort(port);
                        });
                    });
                } else if (err.code === 'EADDRINUSE') {
                    console.log(`[Server] Port ${port} is already in use. Another instance or service might be running.`);
                }
            });
        };

        const startServer = () => {
            const portsToTry = [18889, 18888, 28888, 38888, 48888, 8888];
            portsToTry.forEach(port => {
                try {
                    startServerOnPort(port);
                } catch (e) {
                    console.error(`[Server] Failed to start server on port ${port}:`, e.message);
                }
            });
        };

        try {
            startServer();
        } catch (e) {
            console.error('[Server] Critical exception starting servers:', e.message);
        }

        // Start seeding restoration, download restoration, and cache cleanup
        setTimeout(() => {
            restoreCachedTorrents();
            restoreDownloads();
            startCacheCleanup();
        }, 3000); // Delay to ensure server is ready

        const url = process.argv.find(a => a.startsWith('decco://'));
        if (url) handleLink(url);

        // --- AUTO UPDATE ---
        autoUpdater.logger = require("electron-log");
        autoUpdater.logger.transports.file.level = "info";

        console.log('[Updater] Initializing...');

        autoUpdater.on('checking-for-update', () => {
            console.log('[Updater] Checking for update...');
            autoUpdater.logger.info('[Updater] Checking for update...');
            if (tray) tray.setToolTip(`Decco Engine v${app.getVersion()} - Checking...`);
        });
        autoUpdater.on('update-available', (info) => {
            console.log('[Updater] Update available:', info.version);
            autoUpdater.logger.info(`[Updater] Update available: ${info.version}`);
            // Silent: Removed balloon notification
        });
        autoUpdater.on('update-not-available', (info) => {
            console.log('[Updater] Update not available.');
            autoUpdater.logger.info('[Updater] Update not available.');
            if (tray) tray.setToolTip(`Decco Engine v${app.getVersion()}`);
        });
        autoUpdater.on('error', (err) => {
            console.log('[Updater] Error in auto-updater:', err);
            autoUpdater.logger.error('[Updater] Error:', err);
            // Silent: Removed error balloon
        });
        autoUpdater.on('download-progress', (progressObj) => {
            let log_message = "Download speed: " + progressObj.bytesPerSecond;
            log_message = log_message + ' - Downloaded ' + progressObj.percent + '%';
            log_message = log_message + ' (' + progressObj.transferred + "/" + progressObj.total + ')';
            console.log('[Updater] ' + log_message);
            if (tray) tray.setToolTip(`Downloading: ${Math.round(progressObj.percent)}%`);
        });
        autoUpdater.on('update-downloaded', (info) => {
            console.log('[Updater] Update downloaded. Forcing silent installation immediately.');
            updateReady = true;
            // Install the update silently and restart the app immediately
            autoUpdater.quitAndInstall(true, true);
        });

        autoUpdater.checkForUpdatesAndNotify();

        setInterval(() => {
            console.log('[Updater] Periodic update check...');
            autoUpdater.checkForUpdatesAndNotify();
        }, 1000 * 60 * 60 * 4); // Check every 4 hours
    });
}

// --- SERVER SETUP ---

// Recursively find the largest video file in a directory
function findVideoFile(dir) {
    if (!fs.existsSync(dir)) return null;
    let largestFile = null;
    let largestSize = -1;

    function scan(currentDir) {
        try {
            const files = fs.readdirSync(currentDir);
            for (const file of files) {
                const fullPath = path.join(currentDir, file);
                const stat = fs.statSync(fullPath);
                if (stat.isDirectory()) {
                    scan(fullPath);
                } else if (/\.(mkv|mp4|avi|webm|ts|mov|flv|m4v|3gp|mpg|mpeg|ogv)$/i.test(file)) {
                    if (stat.size > largestSize) {
                        largestSize = stat.size;
                        largestFile = fullPath;
                    }
                }
            }
        } catch (e) {
            console.error(`[findVideoFile] Error reading ${currentDir}:`, e.message);
        }
    }

    scan(dir);
    return largestFile;
}

// Serve a local file using range headers (essential for seeking and FFmpeg)
function serveLocalFile(filePath, req, res) {
    if (!fs.existsSync(filePath)) {
        return res.status(404).send('File not found');
    }

    const stat = fs.statSync(filePath);
    const fileSize = stat.size;
    const range = req.headers.range;

    const ext = path.extname(filePath).toLowerCase();
    let contentType = 'video/mp4';
    if (ext === '.mkv') contentType = 'video/x-matroska';
    else if (ext === '.webm') contentType = 'video/webm';
    else if (ext === '.avi') contentType = 'video/x-msvideo';

    if (!range) {
        res.writeHead(200, {
            'Content-Length': fileSize,
            'Content-Type': contentType
        });
        const stream = fs.createReadStream(filePath);
        stream.pipe(res);
        return;
    }

    const parts = range.replace(/bytes=/, "").split("-");
    const start = parseInt(parts[0], 10);
    const end = parts[1] ? parseInt(parts[1], 10) : fileSize - 1;

    if (start >= fileSize || end >= fileSize) {
        res.writeHead(416, {
            'Content-Range': `bytes */${fileSize}`
        });
        return res.end();
    }

    const chunksize = (end - start) + 1;
    res.writeHead(206, {
        'Content-Range': `bytes ${start}-${end}/${fileSize}`,
        'Accept-Ranges': 'bytes',
        'Content-Length': chunksize,
        'Content-Type': contentType
    });

    const stream = fs.createReadStream(filePath, { start, end });
    stream.pipe(res);

    req.on('close', () => {
        stream.destroy();
    });
}

function isAllowedLiveProxyProtocol(value) {
    return value === 'http:' || value === 'https:';
}

function buildEngineLiveProxyUrl(target) {
    return `http://127.0.0.1:${PORT}/live/proxy?url=${encodeURIComponent(target)}`;
}

function absolutizeLiveProxyLine(line, baseUrl) {
    try {
        return new URL(line, baseUrl).toString();
    } catch {
        return line;
    }
}

function rewriteLiveManifest(manifest, baseUrl) {
    return manifest
        .split('\n')
        .map((rawLine) => {
            const line = rawLine.trim();

            if (!line) return rawLine;
            if (line.startsWith('#EXT-X-KEY')) {
                return rawLine.replace(/URI="([^"]+)"/, (_, uri) => {
                    const absolute = absolutizeLiveProxyLine(uri, baseUrl);
                    return `URI="${buildEngineLiveProxyUrl(absolute)}"`;
                });
            }
            if (line.startsWith('#')) return rawLine;

            const absolute = absolutizeLiveProxyLine(line, baseUrl);
            return buildEngineLiveProxyUrl(absolute);
        })
        .join('\n');
}

const serverApp = express();

// Enable Private Network Access (PNA) and CORS preflight headers for secure contexts
serverApp.use((req, res, next) => {
    if (req.headers['access-control-request-private-network']) {
        res.setHeader('Access-Control-Allow-Private-Network', 'true');
    }
    res.setHeader('Access-Control-Allow-Origin', '*');
    res.setHeader('Access-Control-Allow-Methods', 'GET,POST,PUT,DELETE,OPTIONS');
    res.setHeader('Access-Control-Allow-Headers', 'Content-Type, Range, Authorization, X-Requested-With');
    
    if (req.method === 'OPTIONS') {
        return res.sendStatus(200);
    }
    next();
});
serverApp.use(cors());

// Security middleware: only allow requests from loopback interfaces (localhost/127.0.0.1/::1)
serverApp.use((req, res, next) => {
    const remoteIp = req.socket.remoteAddress;
    if (!remoteIp) {
        return res.status(403).send('Access denied: unidentified connection');
    }
    const isLoopback = remoteIp === '127.0.0.1' || 
                       remoteIp === '::1' || 
                       remoteIp === '::ffff:127.0.0.1' || 
                       remoteIp.startsWith('127.') || 
                       remoteIp.startsWith('::ffff:127.') ||
                       remoteIp.endsWith('127.0.0.1');
    if (!isLoopback) {
        console.warn(`[Security] Blocked non-local request from ${remoteIp}`);
        return res.status(403).send('Access denied: connections allowed only from localhost');
    }
    next();
});

// INTERNAL HTTP PROXY (The "Smart" Layer)
// This translates FFmpeg's Rage requests into Torrent byte reads
serverApp.get('/proxy/:hash', (req, res) => {
    const { hash } = req.params;

    // Check if fully downloaded first (for robustness)
    const allMeta = loadDownloadsMeta();
    const entry = allMeta.downloads[hash];
    if (entry && entry.status === 'completed') {
        const filePath = findVideoFile(entry.downloadDir);
        if (filePath) {
            console.log(`[Proxy] ${hash}: Serving fully downloaded file from local disk: ${filePath}`);
            return serveLocalFile(filePath, req, res);
        }
    }

    const engine = activeEngines.get(hash);
    if (!engine || !engine.videoFile) return res.status(404).end();

    if (engine.isMock) {
        console.log(`[Proxy] ${hash}: Serving from mock local path: ${engine.videoFile.path}`);
        return serveLocalFile(engine.videoFile.path, req, res);
    }

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

// LIVE TV PROXY
// Used only for public http(s) IPTV sources so the browser can read them
// through the local Decco Engine without routing traffic via production.
serverApp.get('/live/proxy', async (req, res) => {
    const target = req.query.url;
    if (!target || typeof target !== 'string') {
        return res.status(400).json({ error: 'Missing url parameter' });
    }

    let parsed;
    try {
        parsed = new URL(target);
    } catch {
        return res.status(400).json({ error: 'Invalid url parameter' });
    }

    if (!isAllowedLiveProxyProtocol(parsed.protocol)) {
        return res.status(400).json({ error: 'Unsupported protocol' });
    }

    try {
        const upstream = await fetch(parsed.toString(), {
            headers: {
                'User-Agent': req.headers['user-agent'] || 'Mozilla/5.0 Decco Engine Live Proxy',
                'Accept': req.headers.accept || '*/*',
            },
            redirect: 'follow',
        });

        if (!upstream.ok) {
            return res.status(upstream.status).json({ error: `Upstream request failed with ${upstream.status}` });
        }

        const contentType = upstream.headers.get('content-type') || '';
        const isManifest =
            parsed.pathname.endsWith('.m3u8') ||
            contentType.includes('application/vnd.apple.mpegurl') ||
            contentType.includes('application/x-mpegurl');

        res.setHeader('Cache-Control', 'no-store');

        if (isManifest) {
            const manifest = await upstream.text();
            const rewritten = rewriteLiveManifest(manifest, parsed.toString());
            res.setHeader('Content-Type', 'application/vnd.apple.mpegurl');
            return res.status(200).send(rewritten);
        }

        if (contentType) {
            res.setHeader('Content-Type', contentType);
        }

        upstream.headers.forEach((value, key) => {
            const lower = key.toLowerCase();
            if (lower === 'content-type' || lower === 'content-length' || lower === 'content-encoding' || lower === 'transfer-encoding') {
                return;
            }
            res.setHeader(key, value);
        });

        const bodyStream = upstream.body ? Readable.fromWeb(upstream.body) : null;
        if (!bodyStream) {
            return res.status(502).json({ error: 'Upstream body unavailable' });
        }

        bodyStream.on('error', (error) => {
            console.error('[Live Proxy] Stream error:', error.message);
            if (!res.headersSent) {
                res.status(502).end();
            } else {
                res.end();
            }
        });

        req.on('close', () => bodyStream.destroy());
        bodyStream.pipe(res);
    } catch (error) {
        console.error('[Live Proxy] Failed:', error.message);
        return res.status(502).json({ error: 'Failed to proxy live stream', details: error.message });
    }
});

// HTTP Trigger to start engine (Bypasses custom protocol issues in Dev)
serverApp.get('/start/:hash', (req, res) => {
    const hash = req.params.hash;
    const fileIdx = req.query.fileIdx !== undefined ? parseInt(req.query.fileIdx, 10) : null;
    const season = req.query.season !== undefined ? parseInt(req.query.season, 10) : null;
    const episode = req.query.episode !== undefined ? parseInt(req.query.episode, 10) : null;

    // Check if fully downloaded first
    const allMeta = loadDownloadsMeta();
    const downloadId = getDownloadId(hash, season, episode, fileIdx);
    const entry = allMeta.downloads[downloadId];
    if (entry && entry.status === 'completed') {
        console.log(`[HTTP Start] ${hash} (ID: ${downloadId}) is fully downloaded. Creating mock engine.`);
        const filePath = findExactVideoFile(entry.downloadDir, entry.fileName);
        if (filePath) {
            const fileName = path.basename(filePath);
            const fileSize = fs.statSync(filePath).size;
            
            const mockEngine = {
                status: 'ready',
                metadataReady: true,
                videoFile: {
                    name: fileName,
                    path: filePath,
                    length: fileSize
                },
                requestedFileIdx: fileIdx,
                files: [{ name: fileName, length: fileSize }],
                duration: entry.duration || 0,
                isMock: true
            };
            
            activeEngines.set(hash, mockEngine);
            
            // Probe local file directly in background for duration
            if (!mockEngine.duration) {
                ffmpeg.ffprobe(filePath, (err, metadata) => {
                    if (!err && metadata.format && metadata.format.duration) {
                        mockEngine.duration = metadata.format.duration;
                        console.log(`[Mock Engine] Precise duration found: ${mockEngine.duration}s`);
                    }
                });
            }
            
            res.json({ status: 'started', hash, fileIdx, season, episode });
            return;
        }
    }

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

serverApp.post('/cache/clear', (req, res) => {
    try {
        clearAllCache();
        res.json({ status: 'ok', cleared: true });
    } catch (error) {
        console.error('[Cache] Failed to clear cache via API:', error);
        res.status(500).json({ status: 'error', cleared: false, error: error.message });
    }
});

serverApp.delete('/cache/clear', (req, res) => {
    try {
        clearAllCache();
        res.json({ status: 'ok', cleared: true });
    } catch (error) {
        console.error('[Cache] Failed to clear cache via API:', error);
        res.status(500).json({ status: 'error', cleared: false, error: error.message });
    }
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

// --- DOWNLOAD ENDPOINTS ---

serverApp.get('/download/start', (req, res) => {
    const { hash, title, imdbId, season, episode, fileIdx } = req.query;
    if (!hash) return res.status(400).json({ error: 'hash is required' });

    const meta = startDownload(
        hash,
        decodeURIComponent(title || 'Video'),
        imdbId || '',
        season ? parseInt(season) : 0,
        episode ? parseInt(episode) : 0,
        fileIdx !== undefined ? parseInt(fileIdx) : null
    );

    res.json({ status: 'started', hash, id: meta.id, title: meta.title });
});

serverApp.get('/download/list', (req, res) => {
    const result = [];
    const allMeta = loadDownloadsMeta();

    // Merge persisted meta with live data from active downloads
    for (const [downloadId, entry] of Object.entries(allMeta.downloads)) {
        const active = activeDownloads.get(downloadId);
        if (active) {
            result.push({
                ...active.meta,
                progress: active.meta.progress,
                speed: active.meta.speed,
                peers: active.meta.peers
            });
        } else {
            // Not actively downloading (completed or stopped)
            result.push({
                ...entry,
                progress: entry.status === 'completed' ? 1.0 : 0,
                speed: 0,
                peers: 0
            });
        }
    }

    res.json({ downloads: result });
});

serverApp.get('/download/pause/:id', (req, res) => {
    const { id } = req.params;
    let active = activeDownloads.get(id);
    if (!active) {
        for (const [key, value] of activeDownloads.entries()) {
            if (value.meta.hash === id) {
                active = value;
                break;
            }
        }
    }
    if (!active) return res.status(404).json({ error: 'Download not found' });

    const downloadId = active.meta.id || id;

    if (active.interval) clearInterval(active.interval);
    if (active.engine) {
        try { active.engine.destroy(); } catch (e) { }
    }
    active.meta.status = 'paused';
    persistDownloadMeta(downloadId, active.meta);
    activeDownloads.delete(downloadId);

    res.json({ status: 'paused', hash: active.meta.hash, id: downloadId });
});

serverApp.get('/download/resume/:id', (req, res) => {
    const { id } = req.params;
    const allMeta = loadDownloadsMeta();
    let saved = allMeta.downloads[id];
    if (!saved) {
        for (const [key, value] of Object.entries(allMeta.downloads)) {
            if (value.hash === id) {
                saved = value;
                break;
            }
        }
    }
    if (!saved) return res.status(404).json({ error: 'Download not found in meta' });

    startDownload(saved.hash, saved.title, saved.imdbId, saved.season, saved.episode, saved.fileIdx);
    res.json({ status: 'resumed', hash: saved.hash, id: saved.id });
});

serverApp.get('/download/delete/:id', (req, res) => {
    const { id } = req.params;

    const allMeta = loadDownloadsMeta();
    let entry = allMeta.downloads[id];
    let downloadId = id;

    if (!entry) {
        for (const [key, value] of Object.entries(allMeta.downloads)) {
            if (value.hash === id) {
                entry = value;
                downloadId = key;
                break;
            }
        }
    }

    // Stop active download if running
    let active = activeDownloads.get(downloadId);
    if (!active) {
        for (const [key, value] of activeDownloads.entries()) {
            if (value.meta.hash === id) {
                active = value;
                downloadId = key;
                break;
            }
        }
    }

    const deleteFiles = () => {
        if (entry && entry.downloadDir) {
            const otherDownloads = Object.values(allMeta.downloads).filter(
                d => d.hash === entry.hash && d.id !== entry.id
            );

            if (otherDownloads.length > 0) {
                try {
                    const filePath = path.join(entry.downloadDir, entry.fileName);
                    if (fs.existsSync(filePath)) {
                        fs.rmSync(filePath, { force: true });
                        console.log(`[Downloads] Deleted single file: ${filePath}`);
                    }
                    if (entry.subtitles) {
                        entry.subtitles.forEach(sub => {
                            if (sub.path && fs.existsSync(sub.path)) {
                                fs.rmSync(sub.path, { force: true });
                            }
                        });
                    }
                } catch (e) {
                    console.log(`[Downloads] Failed to delete specific files for ${id}:`, e.message);
                }
            } else {
                const attemptDelete = (attempt) => {
                    try {
                        if (fs.existsSync(entry.downloadDir)) {
                            fs.rmSync(entry.downloadDir, { recursive: true, force: true });
                            console.log(`[Downloads] Deleted folder successfully on attempt ${attempt}: ${entry.downloadDir}`);
                        }
                    } catch (e) {
                        console.log(`[Downloads] Attempt ${attempt} failed to delete files for ${id}:`, e.message);
                        if (attempt < 5) {
                            const delay = attempt * 1000;
                            console.log(`[Downloads] Scheduling deletion retry attempt ${attempt + 1} in ${delay}ms`);
                            setTimeout(() => attemptDelete(attempt + 1), delay);
                        }
                    }
                };
                attemptDelete(1);
            }
        }
    };

    if (active) {
        if (active.interval) {
            try { clearInterval(active.interval); } catch (e) {}
        }
        if (active.engine) {
            try {
                active.engine.destroy();
                console.log(`[Downloads] Engine destroyed successfully for hash: ${id}`);
            } catch (e) {
                console.log(`[Downloads] Error destroying engine:`, e.message);
            }
        }
        activeDownloads.delete(downloadId);
    }

    // Always delete files from disk
    deleteFiles();

    // Remove from meta
    delete allMeta.downloads[downloadId];
    saveDownloadsMeta(allMeta);

    res.json({ status: 'deleted', hash: entry ? entry.hash : id, id: downloadId });
});

serverApp.get('/download/open/:id', (req, res) => {
    const { id } = req.params;
    const allMeta = loadDownloadsMeta();
    let entry = allMeta.downloads[id];
    if (!entry) {
        for (const [key, value] of Object.entries(allMeta.downloads)) {
            if (value.hash === id) {
                entry = value;
                break;
            }
        }
    }
    if (!entry || !entry.downloadDir) {
        return res.status(404).json({ error: 'Download not found' });
    }

    try {
        const videoFilePath = findExactVideoFile(entry.downloadDir, entry.fileName);
        if (videoFilePath) {
            shell.showItemInFolder(videoFilePath);
        } else {
            shell.openPath(entry.downloadDir);
        }
    } catch (e) {
        shell.openPath(entry.downloadDir);
    }

    res.json({ status: 'opened', hash: entry.hash, id: entry.id });
});
