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
const sseDownloadClients = new Set();

function getDiskSpace() {
    try {
        if (typeof fs.statfsSync === 'function') {
            const stats = fs.statfsSync(DOWNLOADS_DIR);
            const free = Number(stats.bavail) * Number(stats.bsize);
            const total = Number(stats.blocks) * Number(stats.bsize);
            return { free, total, used: total - free };
        }
    } catch (_) {}
    return null;
}

function getDownloadsPayload() {
    const result = [];
    const allMeta = loadDownloadsMeta();
    for (const [downloadId, entry] of Object.entries(allMeta.downloads)) {
        const active = activeDownloads.get(downloadId);
        if (active) {
            result.push({
                ...active.meta,
                progress: active.meta.progress,
                speed: active.meta.speed,
                peers: active.meta.peers,
                eta: active.meta.eta ?? null,
                isStalled: active.meta.isStalled ?? false,
                downloadedBytes: active.meta.downloadedBytes ?? Math.round((active.meta.progress || 0) * (active.meta.fileSize || 0))
            });
        } else {
            result.push({
                ...entry,
                progress: entry.status === 'completed' ? 1.0 : 0,
                speed: 0,
                peers: 0,
                eta: null,
                isStalled: false,
                downloadedBytes: entry.status === 'completed' ? (entry.fileSize || 0) : 0
            });
        }
    }
    const settings = (allMeta && allMeta.settings) || { maxConcurrent: 2, autoPauseOnPlayback: true };
    return {
        downloads: result,
        settings,
        diskSpace: getDiskSpace(),
        downloadDir: DOWNLOADS_DIR
    };
}

let lastBroadcastPayload = '';
function broadcastDownloadEvents() {
    if (sseDownloadClients.size === 0) return;
    try {
        const payload = JSON.stringify(getDownloadsPayload());
        if (payload === lastBroadcastPayload) return;
        lastBroadcastPayload = payload;
        const msg = `data: ${payload}\n\n`;
        for (const client of sseDownloadClients) {
            try {
                client.write(msg);
            } catch (err) {
                sseDownloadClients.delete(client);
            }
        }
    } catch (e) {
        console.error('[SSE] Broadcast error:', e.message);
    }
}

let tray = null;
const activeEngines = new Map();
let activeStreamingHash = null;
let autoResumeTimeout = null;
const stopGraceTimeouts = new Map();

function cancelStopGraceTimeout(hash) {
    if (!hash) return;
    const normalized = String(hash).toLowerCase();
    if (stopGraceTimeouts.has(normalized)) {
        clearTimeout(stopGraceTimeouts.get(normalized));
        stopGraceTimeouts.delete(normalized);
    }
}

function isEngineSharedWithDownloads(hash, engine) {
    for (const activeDl of activeDownloads.values()) {
        if (activeDl.engine === engine || activeDl.meta?.hash === hash) {
            return true;
        }
    }
    return false;
}

function pauseCompetingStreamsAndDownloads(targetHash, targetDownloadId = null) {
    cancelStopGraceTimeout(targetHash);
    activeStreamingHash = targetHash;
    if (autoResumeTimeout) {
        clearTimeout(autoResumeTimeout);
        autoResumeTimeout = null;
    }

    // 1. Stop any other active streaming engines (previous movies/series left mid-watch)
    for (const [otherHash, otherEngine] of activeEngines.entries()) {
        if (otherHash === targetHash) continue;
        cancelStopGraceTimeout(otherHash);

        if (isEngineSharedWithDownloads(otherHash, otherEngine)) {
            activeEngines.delete(otherHash);
            continue;
        }

        try {
            if (!otherEngine.isMock && typeof otherEngine.destroy === 'function') {
                otherEngine.destroy();
                console.log(`[Engine] Paused previous streaming engine for ${otherHash} to prioritize ${targetHash}`);
            }
        } catch (e) {
            console.log(`[Engine] Error stopping previous stream ${otherHash}:`, e.message);
        }
        activeEngines.delete(otherHash);
    }

    // 2. Auto-pause any active background downloads that are not the current playback target
    let pausedAnyDownload = false;
    for (const [dlId, activeDl] of activeDownloads.entries()) {
        if (dlId === targetDownloadId || activeDl.meta?.hash === targetHash) continue;
        if (activeDl.meta && (activeDl.meta.status === 'downloading' || activeDl.meta.status === 'loading')) {
            console.log(`[Downloads] Auto-pausing background download "${activeDl.meta.title}" (${dlId}) to maximize bandwidth for active playback (${targetHash})`);
            if (activeDl.interval) clearInterval(activeDl.interval);
            if (activeDl.stalledTimeout) clearTimeout(activeDl.stalledTimeout);
            if (activeDl.engine) {
                try { activeDl.engine.destroy(); } catch (e) {}
            }
            activeDl.meta.status = 'paused';
            activeDl.meta.autoPausedForStream = true;
            persistDownloadMeta(dlId, activeDl.meta);
            activeDownloads.delete(dlId);
            pausedAnyDownload = true;
        }
    }

    if (pausedAnyDownload) {
        broadcastDownloadEvents();
    }
}

function stopStreamAndResumeAutoPaused(stoppedHash) {
    if (!stoppedHash) return;
    const normalized = String(stoppedHash).toLowerCase();
    cancelStopGraceTimeout(normalized);

    // Debounce stop by 6s so React StrictMode remounts or out-of-order /stop vs /start
    // requests never destroy the active stream the user is currently trying to play.
    const timer = setTimeout(() => {
        stopGraceTimeouts.delete(normalized);

        if (activeEngines.has(stoppedHash)) {
            const eng = activeEngines.get(stoppedHash);
            if (!isEngineSharedWithDownloads(stoppedHash, eng)) {
                try {
                    if (!eng.isMock && typeof eng.destroy === 'function') {
                        eng.destroy();
                        console.log(`[Engine] Stopped streaming engine for ${stoppedHash} (playback exited after grace period)`);
                    }
                } catch (e) {}
                activeEngines.delete(stoppedHash);
            }
        }

        if (activeStreamingHash === stoppedHash) {
            activeStreamingHash = null;
        }

        if (autoResumeTimeout) clearTimeout(autoResumeTimeout);
        autoResumeTimeout = setTimeout(() => {
            autoResumeTimeout = null;
            if (activeStreamingHash) return;

            const allMeta = loadDownloadsMeta();
            let resumedCount = 0;
            for (const [dlId, saved] of Object.entries(allMeta.downloads)) {
                if (saved.status === 'paused' && saved.autoPausedForStream) {
                    console.log(`[Downloads] Auto-resuming download "${saved.title}" (${dlId}) now that playback stopped`);
                    saved.autoPausedForStream = false;
                    saveDownloadsMeta(allMeta);
                    startDownload(saved.hash, saved.title, saved.imdbId, saved.season, saved.episode, saved.fileIdx);
                    resumedCount++;
                }
            }
            if (resumedCount > 0) {
                broadcastDownloadEvents();
            }
        }, 2000);
    }, 6000);

    stopGraceTimeouts.set(normalized, timer);
}

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

let cachedDownloadsMeta = null;
let saveDownloadsMetaTimeout = null;

function loadDownloadsMeta() {
    if (cachedDownloadsMeta) return cachedDownloadsMeta;
    try {
        if (fs.existsSync(DOWNLOADS_META_PATH)) {
            cachedDownloadsMeta = JSON.parse(fs.readFileSync(DOWNLOADS_META_PATH, 'utf-8'));
            if (!cachedDownloadsMeta.settings) cachedDownloadsMeta.settings = { maxConcurrent: 2, autoPauseOnPlayback: true };
            return cachedDownloadsMeta;
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
    cachedDownloadsMeta = { downloads: {}, settings: { maxConcurrent: 2, autoPauseOnPlayback: true } };
    return cachedDownloadsMeta;
}

function saveDownloadsMeta(meta) {
    cachedDownloadsMeta = meta;
    if (saveDownloadsMetaTimeout) return;
    saveDownloadsMetaTimeout = setTimeout(async () => {
        saveDownloadsMetaTimeout = null;
        try {
            const tempPath = `${DOWNLOADS_META_PATH}.tmp.${Date.now()}`;
            await fs.promises.writeFile(tempPath, JSON.stringify(cachedDownloadsMeta, null, 2));
            await fs.promises.rename(tempPath, DOWNLOADS_META_PATH);
        } catch (e) {
            console.log('[Downloads] Error saving meta atomically:', e.message);
            try {
                await fs.promises.writeFile(DOWNLOADS_META_PATH, JSON.stringify(cachedDownloadsMeta, null, 2));
            } catch (err) {
                console.log('[Downloads] Fallback save failed:', err.message);
            }
        }
    }, 500);
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

function pumpDownloadQueue() {
    const allMeta = loadDownloadsMeta();
    const settings = (allMeta && allMeta.settings) || { maxConcurrent: 2, autoPauseOnPlayback: true };
    const maxConcurrent = typeof settings.maxConcurrent === 'number' ? settings.maxConcurrent : 2;
    let currentlyDownloading = 0;
    for (const dl of activeDownloads.values()) {
        if (dl.meta && (dl.meta.status === 'downloading' || dl.meta.status === 'loading')) {
            currentlyDownloading++;
        }
    }
    if (maxConcurrent > 0 && currentlyDownloading >= maxConcurrent) return;
    const availableSlots = maxConcurrent === 0 ? 999 : (maxConcurrent - currentlyDownloading);
    const queuedItems = Object.entries(allMeta.downloads)
        .filter(([id, d]) => d.status === 'queued' && !activeDownloads.has(id))
        .sort((a, b) => (a[1].queuePosition || a[1].startedAt || 0) - (b[1].queuePosition || b[1].startedAt || 0));
    let started = 0;
    for (const [id, item] of queuedItems) {
        if (started >= availableSlots) break;
        console.log('[Queue] Starting queued item:', item.title, id);
        startDownload(item.hash, item.title, item.imdbId, item.season, item.episode, item.fileIdx, true);
        started++;
    }
}

function startDownload(hash, title, imdbId, season, episode, fileIdx, bypassQueue = false) {
    const downloadId = getDownloadId(hash, season, episode, fileIdx);

    if (activeDownloads.has(downloadId)) {
        console.log(`[Downloads] Already downloading: ${downloadId}`);
        return activeDownloads.get(downloadId).meta;
    }

    const allMeta = loadDownloadsMeta();
    const settings = (allMeta && allMeta.settings) || { maxConcurrent: 2, autoPauseOnPlayback: true };
    const maxConcurrent = typeof settings.maxConcurrent === 'number' ? settings.maxConcurrent : 2;
    if (!bypassQueue && maxConcurrent > 0) {
        let currentlyDownloading = 0;
        for (const [key, dl] of activeDownloads.entries()) {
            if (key !== downloadId && dl.meta && (dl.meta.status === 'downloading' || dl.meta.status === 'loading')) {
                currentlyDownloading++;
            }
        }
        if (currentlyDownloading >= maxConcurrent) {
            console.log('[Downloads] Concurrency limit reached (' + currentlyDownloading + '/' + maxConcurrent + '). Queueing ' + title);
            const existing = allMeta.downloads[downloadId] || {};
            const meta = {
                ...existing,
                id: downloadId,
                hash,
                title: title || existing.title || 'Unknown',
                imdbId: imdbId || existing.imdbId || '',
                season: season || existing.season || 0,
                episode: episode || existing.episode || 0,
                fileIdx: fileIdx !== undefined ? fileIdx : existing.fileIdx,
                status: 'queued',
                progress: existing.progress || 0,
                speed: 0,
                peers: 0,
                eta: null,
                isStalled: false,
                downloadDir: path.join(DOWNLOADS_DIR, hash.substring(0, 16)),
                startedAt: existing.startedAt || Date.now(),
                queuePosition: existing.queuePosition || Date.now()
            };
            persistDownloadMeta(downloadId, meta);
            broadcastDownloadEvents();
            return meta;
        }
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

        // Move already cached files to the permanent download directory so progress is preserved without blocking the thread
        if (srcPath && fs.existsSync(srcPath)) {
            try {
                console.log(`[Downloads] Moving cached files from "${srcPath}" to "${downloadDir}"...`);
                const entries = fs.readdirSync(srcPath);
                for (const entryName of entries) {
                    const srcItem = path.join(srcPath, entryName);
                    const destItem = path.join(downloadDir, entryName);
                    try {
                        if (fs.existsSync(destItem)) {
                            fs.rmSync(destItem, { recursive: true, force: true });
                        }
                        fs.renameSync(srcItem, destItem);
                    } catch (renameErr) {
                        // Cross-device fallback
                        fs.cpSync(srcItem, destItem, { recursive: true, force: true, errorOnExist: false });
                    }
                }
                console.log(`[Downloads] Moved cached files successfully.`);

                // Clean up the temporary streaming folder now that contents are moved to completed-downloads
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
                console.error(`[Downloads] Failed to move cached files:`, e.message);
            }
        }
    }

    console.log(`[Downloads] Starting download: ${title} (${hash}) [ID: ${downloadId}]`);

    // Asynchronously warm .torrent metadata cache from HTTP if not yet on disk
    prefetchTorrentCacheFile(hash, downloadDir).catch(() => {});

    const engine = torrentStream(`magnet:?xt=urn:btih:${hash}&tr=${TRACKERS.map(encodeURIComponent).join('&tr=')}`, {
        tmp: downloadDir,
        path: downloadDir,
        trackers: TRACKERS,
        dht: getSharedDht(),
        connections: 150,
        uploads: 8
    });

    if (typeof engine.listen === 'function') {
        engine.listen(0, () => {
            console.log(`[Downloads] Swarm listening on port ${engine.port} for ${hash}`);
        });
    }

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

        // Stalled detection timer (if no peers or metadata for 45s)
        let stalledTimeout = setTimeout(() => {
            if (meta.status === 'downloading' && meta.speed === 0 && meta.peers === 0 && meta.progress === 0) {
                meta.isStalled = true;
                broadcastDownloadEvents();
            }
        }, 45000);

        // Progress polling
        const interval = setInterval(() => {
            const progress = getFileProgress(engine, file);
            meta.progress = progress;
            meta.speed = engine.swarm ? engine.swarm.downloadSpeed() : 0;
            meta.peers = engine.swarm ? engine.swarm.wires.length : 0;
            meta.downloadedBytes = Math.round(progress * (meta.fileSize || 0));

            if (meta.speed > 0 || meta.peers > 0 || progress > 0) {
                meta.isStalled = false;
            }

            // Estimate Remaining Time (ETA in seconds)
            if (meta.speed > 0 && meta.fileSize > 0 && progress < 1.0) {
                const remainingBytes = meta.fileSize - meta.downloadedBytes;
                meta.eta = Math.max(0, Math.round(remainingBytes / meta.speed));
            } else {
                meta.eta = null;
            }

            if (progress >= 1.0 && meta.status === 'downloading') {
                meta.status = 'completed';
                meta.completedAt = Date.now();
                meta.progress = 1.0;
                meta.eta = 0;
                if (stalledTimeout) clearTimeout(stalledTimeout);
                clearInterval(interval);
                console.log(`[Downloads] COMPLETED: ${meta.title}`);
                persistDownloadMeta(downloadId, meta);
                broadcastDownloadEvents();

                // Clean up unwanted video files from multipack/season-pack torrents.
                // BitTorrent pieces can span file boundaries, so downloading one episode
                // may cause adjacent episodes' data to also be written to disk.
                cleanupUnwantedFiles(downloadDir, file.name);
                pumpDownloadQueue(); // after comp
            } else {
                broadcastDownloadEvents();
            }
        }, 1000);

        activeDownloads.set(downloadId, { engine, meta, interval, file, stalledTimeout });
        persistDownloadMeta(downloadId, meta);
        broadcastDownloadEvents();
    });

    engine.on('error', (err) => {
        meta.status = 'error';
        meta.lastError = err.message;
        console.error(`[Downloads] Error for ${hash}:`, err.message);
        persistDownloadMeta(downloadId, meta);
        broadcastDownloadEvents();
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
        autoPausedForStream: Boolean(meta.autoPausedForStream),
        startedAt: meta.startedAt,
        completedAt: meta.completedAt
    };
    saveDownloadsMeta(allMeta);
}

function restoreDownloads() {
    const allMeta = loadDownloadsMeta();
    const downloadIds = Object.keys(allMeta.downloads);
    console.log(`[Downloads] Restoring active downloads from ${downloadIds.length} saved entries...`);
    downloadIds.forEach(id => {
        const saved = allMeta.downloads[id];
        // Only spin up active P2P engines for in-progress downloads (or ones temporarily auto-paused for streaming)
        if (saved.status === 'downloading' || saved.status === 'loading' || (saved.status === 'paused' && saved.autoPausedForStream)) {
            saved.autoPausedForStream = false;
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
    'udp://tracker.opentrackr.org:1337/announce',
    'udp://open.demonii.com:1337/announce',
    'udp://exodus.desync.com:6969/announce',
    'udp://open.stealth.si:80/announce',
    'udp://tracker.torrent.eu.org:451/announce',
    'udp://tracker.openbittorrent.com:6969/announce',
    'udp://tracker.moeking.me:6969/announce',
    'udp://explodie.org:6969/announce',
    'udp://tracker1.bt.moack.co.kr:80/announce',
    'udp://tracker.theoks.net:6969/announce',
    'udp://tracker.tiny-vps.com:6969/announce',
    'udp://tracker.dler.org:6969/announce',
    'udp://opentracker.io:6969/announce',
    'udp://open.free-tracker.ga:6969/announce',
    'udp://opentor.net:6969',
    'udp://p4p.arenabg.com:1337/announce',
    'udp://movies.zsw.ca:6969/announce',
    'http://tracker.opentrackr.org:1337/announce',
    'http://tracker.openbittorrent.com:80/announce',
    'http://open.acgnxtracker.com:80/announce',
    'http://tracker.dler.org:6969/announce',
    'https://tracker.tamersunion.org:443/announce',
    'https://tracker.gbitt.info:443/announce',
    'http://bt.t-ru.org/ann?magnet',
    'http://bt2.t-ru.org/ann?magnet',
    'http://bt3.t-ru.org/ann?magnet',
    'http://bt4.t-ru.org/ann?magnet'
];

// --- PERSISTENT DHT & METADATA CACHE ACCELERATION ---
let sharedDht = null;
function getSharedDht() {
    if (sharedDht) return sharedDht;
    try {
        const DHT = require('bittorrent-dht/client');
        if (typeof DHT === 'function') {
            sharedDht = new DHT();
            sharedDht.listen(0, () => {
                console.log('[DHT] Shared persistent DHT node listening and pre-warming routing table');
            });
            sharedDht.on('error', (err) => {
                console.log('[DHT] Warning:', err.message);
            });
        }
    } catch (e) {
        console.log('[DHT] Could not initialize shared DHT:', e.message);
    }
    return sharedDht || true;
}
// Pre-warm DHT immediately on module load
getSharedDht();

// Pre-fetch .torrent metadata from public HTTP caches if not already cached locally
async function prefetchTorrentCacheFile(hash, targetDir) {
    if (!hash) return null;
    const cleanHash = String(hash).trim().toLowerCase();
    const upperHash = cleanHash.toUpperCase();
    const cacheTorrentDir = path.join(targetDir, 'torrent-stream');
    const cacheTorrentPath = path.join(cacheTorrentDir, `${cleanHash}.torrent`);

    try {
        if (fs.existsSync(cacheTorrentPath) && fs.statSync(cacheTorrentPath).size > 64) {
            return cacheTorrentPath;
        }
    } catch (_) {}

    const urls = [
        `https://itorrents.org/torrent/${upperHash}.torrent`,
        `https://torrage.info/torrent.php?h=${upperHash}`
    ];

    for (const url of urls) {
        try {
            const res = await fetch(url, {
                headers: {
                    'User-Agent': 'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36'
                },
                redirect: 'follow',
                signal: AbortSignal.timeout(2200)
            });
            if (!res.ok) continue;
            const ab = await res.arrayBuffer();
            const buf = Buffer.from(ab);
            // Valid bencoded .torrent starts with 'd' (0x64)
            if (buf.length > 64 && buf[0] === 0x64) {
                fs.mkdirSync(cacheTorrentDir, { recursive: true });
                fs.writeFileSync(cacheTorrentPath, buf);
                console.log(`[Engine] Fast-resolved .torrent metadata via HTTP cache (${buf.length} bytes) for ${cleanHash}`);
                return cacheTorrentPath;
            }
        } catch (_) {
            // Ignore timeout/network errors and fall back to DHT/tracker magnet resolution
        }
    }
    return null;
}

// Detect hardware H.264 encoder for fast 4K HEVC transcoding while preserving original resolution
let preferredH264Encoder = null;
function detectHardwareEncoder() {
    if (preferredH264Encoder !== null) return preferredH264Encoder;
    preferredH264Encoder = 'libx264'; // Default fallback while probing
    try {
        ffmpeg.getAvailableEncoders((err, encoders) => {
            if (err || !encoders) return;
            if (encoders['h264_nvenc']) {
                preferredH264Encoder = 'h264_nvenc';
            } else if (encoders['h264_qsv']) {
                preferredH264Encoder = 'h264_qsv';
            } else if (encoders['h264_amf']) {
                preferredH264Encoder = 'h264_amf';
            }
            console.log(`[FFmpeg] Selected H.264 encoder: ${preferredH264Encoder}`);
        });
    } catch (e) {
        console.log('[FFmpeg] Encoder detection skipped:', e.message);
    }
    return preferredH264Encoder;
}
detectHardwareEncoder();

if (!fs.existsSync(DOWNLOAD_PATH)) fs.mkdirSync(DOWNLOAD_PATH, { recursive: true });

// --- CACHE METADATA MANAGEMENT ---

let cachedCacheMeta = null;
let saveCacheTimeout = null;

function loadCacheMeta() {
    if (cachedCacheMeta) return cachedCacheMeta;
    try {
        if (fs.existsSync(CACHE_META_PATH)) {
            cachedCacheMeta = JSON.parse(fs.readFileSync(CACHE_META_PATH, 'utf-8'));
            if (!cachedCacheMeta || typeof cachedCacheMeta.torrents !== 'object') {
                cachedCacheMeta = { torrents: {} };
            }
            return cachedCacheMeta;
        }
    } catch (e) {
        console.log('[Cache] Error loading meta:', e.message);
    }
    cachedCacheMeta = { torrents: {} };
    return cachedCacheMeta;
}

function saveCacheMeta(meta) {
    cachedCacheMeta = meta;
    if (saveCacheTimeout) clearTimeout(saveCacheTimeout);
    saveCacheTimeout = setTimeout(async () => {
        saveCacheTimeout = null;
        try {
            const tmpPath = `${CACHE_META_PATH}.tmp`;
            await fs.promises.writeFile(tmpPath, JSON.stringify(cachedCacheMeta, null, 2), 'utf-8');
            await fs.promises.rename(tmpPath, CACHE_META_PATH);
        } catch (e) {
            console.log('[Cache] Error saving meta:', e.message);
        }
    }, 2000);
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
        const now = Date.now();
        // Only schedule disk write if more than 60s have passed since last update
        const shouldPersist = !meta.torrents[hash].lastAccessed || (now - meta.torrents[hash].lastAccessed > 60000);
        meta.torrents[hash].lastAccessed = now;
        if (shouldPersist) {
            saveCacheMeta(meta);
        }
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
    // Do not spin up previous streaming torrents on startup so new streams get 100% bandwidth.
    // Cached files on disk are still preserved for 72h if the user resumes watching them.
    const meta = loadCacheMeta();
    console.log(`[Cache] ${Object.keys(meta.torrents).length} cached streaming torrents available on disk (idle until played).`);
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

function prioritizeFileBoundaryPieces(engine, file) {
    if (!engine || !file || engine.isMock) return;
    try {
        const pieceLength = (engine.torrent && engine.torrent.pieceLength) || engine.pieceLength;
        if (pieceLength && pieceLength > 0 && typeof engine.critical === 'function') {
            const startPiece = Math.floor(file.offset / pieceLength);
            const endPiece = Math.floor((file.offset + file.length - 1) / pieceLength);
            // Prioritize ~24 MB at the head of the file (minimum 2 pieces, max 8) so segment-0.ts starts immediately
            const targetHeadPieces = Math.max(2, Math.min(8, Math.ceil((24 * 1024 * 1024) / pieceLength)));
            const headWidth = Math.min(targetHeadPieces, Math.max(1, endPiece - startPiece + 1));
            engine.critical(startPiece, headWidth);
            console.log(`[Engine] Prioritized head pieces: [${startPiece} (x${headWidth}, pieceLength=${(pieceLength / 1024 / 1024).toFixed(2)}MB)]`);
        }
    } catch (e) {
        console.log('[Engine] Error prioritizing boundary pieces:', e.message);
    }
}

function applyProbeMetadata(engine, metadata) {
    if (!engine || !metadata) return;
    if (metadata.format && metadata.format.duration) {
        engine.duration = metadata.format.duration;
    }
    if (Array.isArray(metadata.streams)) {
        const videoStream = metadata.streams.find(s => s.codec_type === 'video');
        if (videoStream) {
            engine.videoCodec = videoStream.codec_name;
        }
        const audioStream = metadata.streams.find(s => s.codec_type === 'audio');
        if (audioStream) {
            engine.audioCodec = audioStream.codec_name;
        }
        engine.embeddedSubtitles = metadata.streams
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
    }
}

function getEngine(hash, fileIdx = null, season = null, episode = null) {
    const downloadId = getDownloadId(hash, season, episode, fileIdx);
    pauseCompetingStreamsAndDownloads(hash, downloadId);

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
                    prioritizeFileBoundaryPieces(existingEngine, correctFile);
                    console.log(`[Engine] Corrected file selection to: ${correctFile.name} - Other files deselected`);
                }
            }
        }
        return existingEngine;
    }

    // Check downloads database first
    const allMeta = loadDownloadsMeta();
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
                
                // Probe local file in background for duration and codecs
                if (!mockEngine.duration || !mockEngine.videoCodec) {
                    ffmpeg.ffprobe(`http://127.0.0.1:${PORT}/proxy/${hash}`, ['-seekable', '0', '-probesize', '5000000', '-analyzeduration', '5000000'], (err, metadata) => {
                        if (!err && metadata) {
                            applyProbeMetadata(mockEngine, metadata);
                            console.log(`[Mock Engine] Probed duration: ${mockEngine.duration}s | video: ${mockEngine.videoCodec} | audio: ${mockEngine.audioCodec}`);
                            
                            // Save duration in persistent downloads metadata
                            try {
                                const freshMeta = loadDownloadsMeta();
                                const savedItem = freshMeta.downloads[downloadId];
                                if (savedItem && mockEngine.duration) {
                                    savedItem.duration = mockEngine.duration;
                                    saveDownloadsMeta(freshMeta);
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
                        if (!err && metadata && metadata.format && metadata.format.duration) {
                            applyProbeMetadata(dlEngine, metadata);
                            console.log(`[DL Engine] Codec Detected: ${dlEngine.videoCodec} / ${dlEngine.audioCodec} | Duration: ${dlEngine.duration}s`);
                            
                            // Save duration in persistent downloads metadata
                            try {
                                const freshMeta = loadDownloadsMeta();
                                const savedItem = freshMeta.downloads[downloadId];
                                if (savedItem) {
                                    savedItem.duration = dlEngine.duration;
                                    saveDownloadsMeta(freshMeta);
                                }
                            } catch (saveErr) {
                                console.error('[DL Engine] Failed to save duration to database:', saveErr.message);
                            }
                        } else if (dlEngine.status === 'ready') {
                            setTimeout(probeDl, 3000);
                        }
                    });
                };
                
                if (dlEngine.videoFile) {
                    dlEngine.metadataReady = true;
                    dlEngine.status = 'ready';
                    prioritizeFileBoundaryPieces(dlEngine, dlEngine.videoFile);
                    probeDl();
                } else {
                    dlEngine.on('ready', () => {
                        dlEngine.metadataReady = true;
                        dlEngine.status = 'ready';
                        if (dlEngine.videoFile) {
                            prioritizeFileBoundaryPieces(dlEngine, dlEngine.videoFile);
                        }
                        probeDl();
                    });
                }
                
                activeEngines.set(hash, dlEngine);
                return dlEngine;
            }
        }
    }

    console.log(`[Engine] Creating for hash: ${hash}, fileIdx: ${fileIdx}, S${season}E${episode}`);
    // Asynchronously warm .torrent metadata cache from HTTP if not yet on disk
    prefetchTorrentCacheFile(hash, DOWNLOAD_PATH).catch(() => {});

    const engine = torrentStream(`magnet:?xt=urn:btih:${hash}&tr=${TRACKERS.map(encodeURIComponent).join('&tr=')}`, {
        tmp: DOWNLOAD_PATH,
        trackers: TRACKERS,
        dht: getSharedDht(),
        connections: 150,
        uploads: 8
    });

    if (typeof engine.listen === 'function') {
        engine.listen(0, () => {
            console.log(`[Engine] Swarm listening on port ${engine.port} for ${hash}`);
        });
    }

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
        prioritizeFileBoundaryPieces(engine, file);
        console.log(`[Engine] Selected ONLY: ${file.name} (${(file.length / 1024 / 1024).toFixed(1)} MB) - Other ${engine.files.length - 1} files deselected`);

        // Background Duration & Codec Probe (via Proxy for speed)
        const probe = () => {
            if (engine.isProbing) return;
            engine.isProbing = true;
            // Probe using the local proxy to test it simultaneously
            ffmpeg.ffprobe(`http://127.0.0.1:${PORT}/proxy/${hash}`, ['-seekable', '0', '-probesize', '5000000', '-analyzeduration', '5000000'], (err, metadata) => {
                engine.isProbing = false;
                if (!err && metadata && metadata.format && metadata.format.duration) {
                    applyProbeMetadata(engine, metadata);
                    console.log(`[Engine] Codec Detected: ${engine.videoCodec} / ${engine.audioCodec} | Duration: ${engine.duration}s`);
                } else if (engine.status === 'ready') {
                    // Retry faster (3s instead of 5s) since boundary pieces are prioritized
                    setTimeout(probe, 3000);
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
        }, 1000 * 60 * 60 * 1); // Check every 1 hour
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

function buildEngineLiveProxyUrl(target, req, opts) {
    const host = req && req.headers && req.headers.host ? req.headers.host : `127.0.0.1:${PORT}`;
    const proxied = new URL(`http://${host}/live/proxy`);
    proxied.searchParams.set('url', target);
    if (opts && opts.referrer) proxied.searchParams.set('ref', opts.referrer);
    if (opts && opts.userAgent) proxied.searchParams.set('ua', opts.userAgent);
    return proxied.toString();
}

function absolutizeLiveProxyLine(line, baseUrl) {
    try {
        return new URL(line, baseUrl).toString();
    } catch {
        return line;
    }
}

function rewriteLiveManifest(manifest, baseUrl, req, opts) {
    return manifest
        .split('\n')
        .map((rawLine) => {
            const line = rawLine.trim();

            if (!line) return rawLine;
            if (line.startsWith('#')) {
                if (rawLine.includes('URI="')) {
                    return rawLine.replace(/URI="([^"]+)"/g, (_, uri) => {
                        if (!uri || uri.startsWith('data:') || uri.startsWith('blob:')) return `URI="${uri}"`;
                        const absolute = absolutizeLiveProxyLine(uri, baseUrl);
                        return `URI="${buildEngineLiveProxyUrl(absolute, req, opts)}"`;
                    });
                }
                return rawLine;
            }

            const absolute = absolutizeLiveProxyLine(line, baseUrl);
            return buildEngineLiveProxyUrl(absolute, req, opts);
        })
        .join('\n');
}

function rewriteLiveDashManifest(manifest, baseUrl, req, opts) {
    return manifest
        .replace(/<(BaseURL)>([^<]+)<\/BaseURL>/g, (match, tag, value) => {
            if (!value || value.startsWith('data:') || value.startsWith('blob:')) {
                return match;
            }
            const absolute = absolutizeLiveProxyLine(value, baseUrl);
            return `<${tag}>${buildEngineLiveProxyUrl(absolute, req, opts)}</${tag}>`;
        })
        .replace(/(media|initialization|sourceURL|href)="([^"]+)"/g, (match, attribute, value) => {
            if (!value || value.startsWith('data:') || value.startsWith('blob:')) {
                return match;
            }
            const absolute = absolutizeLiveProxyLine(value, baseUrl);
            return `${attribute}="${buildEngineLiveProxyUrl(absolute, req, opts)}"`;
        });
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

// Fast, zero-disk-I/O health check endpoint for web discovery
serverApp.get('/ping', (req, res) => {
    res.json({ status: 'ok', port: PORT, version: '2.2' });
});

function getVideoContentType(fileName) {
    if (!fileName) return 'video/mp4';
    const ext = path.extname(fileName).toLowerCase();
    if (ext === '.mkv') return 'video/x-matroska';
    if (ext === '.webm') return 'video/webm';
    if (ext === '.avi') return 'video/x-msvideo';
    return 'video/mp4';
}

// INTERNAL HTTP PROXY (The "Smart" Layer)
// This translates FFmpeg's Rage requests into Torrent byte reads
serverApp.get('/proxy/:hash', (req, res) => {
    const { hash } = req.params;
    cancelStopGraceTimeout(hash);

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
    const contentType = getVideoContentType(file.name);

    if (!range) {
        // Fallback for non-ranged requests (metadata probing)
        prioritizeFileBoundaryPieces(engine, file);
        const stream = file.createReadStream();
        res.writeHead(200, {
            'Content-Length': file.length,
            'Accept-Ranges': 'bytes',
            'Content-Type': contentType
        });
        stream.pipe(res);
        req.on('close', () => stream.destroy());
        return;
    }

    const positions = range.replace(/bytes=/, "").split("-");
    const start = parseInt(positions[0], 10);
    const end = positions[1] ? parseInt(positions[1], 10) : file.length - 1;
    const chunksize = (end - start) + 1;

    // Sliding Readahead Window: dynamically mark pieces covering [start, start + 24MB] as critical
    try {
        const pieceLength = (engine.torrent && engine.torrent.pieceLength) || engine.pieceLength;
        if (pieceLength && pieceLength > 0 && typeof engine.critical === 'function') {
            const currentPiece = Math.floor((file.offset + start) / pieceLength);
            const fileEndPiece = Math.floor((file.offset + file.length - 1) / pieceLength);
            const readaheadPieces = Math.max(2, Math.min(8, Math.ceil((24 * 1024 * 1024) / pieceLength)));
            const width = Math.min(readaheadPieces, Math.max(1, fileEndPiece - currentPiece + 1));
            engine.critical(currentPiece, width);
        }
    } catch (_) {}

    console.log(`[Proxy] ${hash}: Serving bytes ${start}-${end} (${chunksize})`);

    res.writeHead(206, {
        "Content-Range": "bytes " + start + "-" + end + "/" + file.length,
        "Accept-Ranges": "bytes",
        "Content-Length": chunksize,
        "Content-Type": contentType
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
    const customReferrer = typeof req.query.ref === 'string' && req.query.ref ? req.query.ref : undefined;
    const customUserAgent = typeof req.query.ua === 'string' && req.query.ua ? req.query.ua : undefined;
    const shouldProbe = req.query.probe === '1';

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

    const forwardOpts = {
        referrer: customReferrer,
        userAgent: customUserAgent,
    };

    try {
        const requestHeaders = {
            'User-Agent': customUserAgent || req.headers['user-agent'] || 'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36',
            'Accept': req.headers.accept || '*/*',
        };

        if (customReferrer) {
            requestHeaders['Referer'] = customReferrer;
            try {
                requestHeaders['Origin'] = new URL(customReferrer).origin;
            } catch {
                // ignore invalid origin
            }
        }

        if (req.headers.range && !shouldProbe) {
            requestHeaders['Range'] = req.headers.range;
        }

        let upstream = await fetch(parsed.toString(), {
            method: shouldProbe ? 'HEAD' : 'GET',
            headers: requestHeaders,
            redirect: 'follow',
            signal: AbortSignal.timeout(12000),
        });

        if (!upstream.ok && (upstream.status === 403 || upstream.status === 401) && !customReferrer) {
            const spoofedHeaders = {
                ...requestHeaders,
                'Referer': `${parsed.origin}/`,
                'Origin': parsed.origin,
            };
            upstream = await fetch(parsed.toString(), {
                method: shouldProbe ? 'HEAD' : 'GET',
                headers: spoofedHeaders,
                redirect: 'follow',
                signal: AbortSignal.timeout(10000),
            });
            if (upstream.ok) {
                forwardOpts.referrer = `${parsed.origin}/`;
            }
        }

        if (shouldProbe && !upstream.ok && (upstream.status === 403 || upstream.status === 405)) {
            upstream = await fetch(parsed.toString(), {
                method: 'GET',
                headers: {
                    ...requestHeaders,
                    ...(forwardOpts.referrer ? { 'Referer': forwardOpts.referrer } : {}),
                    'Range': 'bytes=0-511',
                },
                redirect: 'follow',
                signal: AbortSignal.timeout(10000),
            });
        }

        if (!upstream.ok) {
            return res.status(upstream.status).json({ error: `Upstream request failed with ${upstream.status}` });
        }

        const contentType = upstream.headers.get('content-type') || '';
        const effectiveBaseUrl = upstream.url || parsed.toString();
        let effectivePathname = parsed.pathname.toLowerCase();
        try {
            effectivePathname = new URL(effectiveBaseUrl).pathname.toLowerCase();
        } catch {
            // keep parsed.pathname
        }

        res.setHeader('Cache-Control', 'no-store');

        if (shouldProbe) {
            let sampleText = '';
            let firstBytes = '';
            if (upstream.body && upstream.headers.get('content-length') !== '0') {
                try {
                    const buf = await upstream.arrayBuffer();
                    const bytes = new Uint8Array(buf.slice(0, 512));
                    sampleText = Buffer.from(bytes).toString('utf8');
                    firstBytes = Buffer.from(bytes.slice(0, 16)).toString('hex');
                } catch {
                    // ignore sample read error
                }
            }
            return res.json({
                contentType,
                finalUrl: effectiveBaseUrl,
                status: upstream.status,
                firstBytes,
                sampleText,
            });
        }

        const isKnownHls =
            effectivePathname.endsWith('.m3u8') ||
            effectivePathname.endsWith('.m3u') ||
            contentType.toLowerCase().includes('mpegurl');
        const isKnownDash =
            effectivePathname.endsWith('.mpd') ||
            contentType.toLowerCase().includes('application/dash+xml');

        if (isKnownHls) {
            const manifest = await upstream.text();
            const rewritten = rewriteLiveManifest(manifest, effectiveBaseUrl, req, forwardOpts);
            res.setHeader('Content-Type', 'application/vnd.apple.mpegurl');
            return res.status(200).send(rewritten);
        }

        if (isKnownDash) {
            const manifest = await upstream.text();
            const rewritten = rewriteLiveDashManifest(manifest, effectiveBaseUrl, req, forwardOpts);
            res.setHeader('Content-Type', 'application/dash+xml');
            return res.status(200).send(rewritten);
        }

        const contentLength = parseInt(upstream.headers.get('content-length') || '0', 10);
        const mightBeTextManifest =
            contentType.startsWith('text/') ||
            contentType.includes('xml') ||
            (contentLength > 0 && contentLength < 256 * 1024 && !effectivePathname.endsWith('.ts') && !effectivePathname.endsWith('.m4s'));

        if (mightBeTextManifest) {
            const clone = upstream.clone();
            const text = await clone.text();
            const trimmed = text.trimStart();
            if (trimmed.startsWith('#EXTM3U')) {
                const rewritten = rewriteLiveManifest(text, effectiveBaseUrl, req, forwardOpts);
                res.setHeader('Content-Type', 'application/vnd.apple.mpegurl');
                return res.status(200).send(rewritten);
            }
            if ((trimmed.startsWith('<?xml') && trimmed.includes('<MPD')) || trimmed.startsWith('<MPD')) {
                const rewritten = rewriteLiveDashManifest(text, effectiveBaseUrl, req, forwardOpts);
                res.setHeader('Content-Type', 'application/dash+xml');
                return res.status(200).send(rewritten);
            }
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

// Pre-warm endpoint: warm up .torrent HTTP cache and prepare engine before user clicks Play
serverApp.get('/prewarm/:hash', async (req, res) => {
    const { hash } = req.params;
    if (!hash) return res.status(400).json({ error: 'Missing hash' });
    cancelStopGraceTimeout(hash);
    prefetchTorrentCacheFile(hash, DOWNLOAD_PATH).catch(() => {});
    if (!activeEngines.has(hash)) {
        try {
            getEngine(hash);
        } catch (_) {}
    }
    res.json({ status: 'prewarming', hash });
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

serverApp.get('/stop/:hash', (req, res) => {
    const { hash } = req.params;
    console.log(`[HTTP Stop] Stopping stream for hash: ${hash}`);
    stopStreamAndResumeAutoPaused(hash);
    res.json({ status: 'stopped', hash });
});

serverApp.post('/stop/:hash', (req, res) => {
    const { hash } = req.params;
    console.log(`[HTTP Stop] Stopping stream for hash: ${hash}`);
    stopStreamAndResumeAutoPaused(hash);
    res.json({ status: 'stopped', hash });
});

serverApp.get('/status/:hash', (req, res) => {
    const hash = req.params.hash;
    cancelStopGraceTimeout(hash);
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
        videoCodec: engine.videoCodec || null,
        audioCodec: engine.audioCodec || null,
        subtitles: engine.embeddedSubtitles || null,
        progress: engine.isMock ? 1 : (engine.videoFile ? getFileProgress(engine, engine.videoFile) : 0),
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
    cancelStopGraceTimeout(hash);
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
    cancelStopGraceTimeout(hash);
    const engine = activeEngines.get(hash);
    if (!engine || !engine.videoFile) return res.status(404).end();

    const segIndex = parseInt(index, 10);
    const startTime = segIndex * SEGMENT_DURATION;

    // Smart Transcode Logic
    const isH264 = engine.videoCodec === 'h264';
    const hwEncoder = detectHardwareEncoder();
    const transcodeMode = isH264 ? 'Direct Remux' : `Transcoding (${hwEncoder})`;

    console.log(`[HLS] Parsing seg ${segIndex} (${startTime}s) | Codec: ${engine.videoCodec || 'Unknown'} | Mode: ${transcodeMode}`);

    res.setHeader('Content-Type', 'video/mp2t');

    // For segment 0 (startTime === 0), pass -seekable 0 so FFmpeg streams sequentially from byte 0
    // without jumping to the end of 20GB+ MKV files to parse Cues/Indexes before outputting frames.
    const inputOpts = startTime > 0
        ? [`-ss ${startTime}`]
        : ['-seekable 0'];

    const ffmpegCommand = ffmpeg(`http://127.0.0.1:${PORT}/proxy/${hash}`)
        .inputOptions(inputOpts);

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
    } else if (hwEncoder === 'h264_nvenc') {
        outputOptions.push('-pix_fmt yuv420p', '-c:v h264_nvenc', '-preset p1', '-cq 23', '-g 48');
    } else if (hwEncoder === 'h264_qsv') {
        outputOptions.push('-pix_fmt nv12', '-c:v h264_qsv', '-preset veryfast', '-global_quality 23', '-g 48');
    } else if (hwEncoder === 'h264_amf') {
        outputOptions.push('-pix_fmt yuv420p', '-c:v h264_amf', '-quality speed', '-g 48');
    } else {
        // HEVC / 4K / Other = Preserve original resolution (4K stays 4K) with ultrafast 8-bit H.264
        outputOptions.push('-pix_fmt yuv420p');
        outputOptions.push('-c:v libx264');
        outputOptions.push('-preset ultrafast');
        outputOptions.push('-threads 0');
        outputOptions.push('-crf 23');
        outputOptions.push('-g 48');
    }

    let commandFinished = false;
    req.on('close', () => {
        if (!commandFinished) {
            try {
                ffmpegCommand.kill('SIGKILL');
            } catch (e) {}
        }
    });

    ffmpegCommand
        .outputOptions(outputOptions)
        .on('start', (cmd) => console.log(`[FFmpeg] Started: ${cmd}`))
        .on('end', () => {
            commandFinished = true;
        })
        .on('stderr', (line) => {
            if (line.includes('Error') || line.includes('warn') || line.includes('fail')) {
                console.log(`[FFmpeg-Log] ${line}`);
            }
        })
        .on('error', (err) => {
            commandFinished = true;
            if (!err.message.includes('SIGKILL') && !err.message.includes('404')) {
                console.error(`[HLS-FFmpeg] Critical Error:`, err.message);
            }
        })
        .pipe(res, { end: true });
});

// --- SUBTITLE ENDPOINTS ---

// List embedded subtitle tracks using FFprobe (or cached probe result)
serverApp.get('/subtitles/:hash', async (req, res) => {
    const { hash } = req.params;
    const engine = activeEngines.get(hash);

    if (!engine || !engine.videoFile) {
        return res.status(404).json({ error: 'Engine or video file not found' });
    }

    if (Array.isArray(engine.embeddedSubtitles)) {
        return res.json({ subtitles: engine.embeddedSubtitles });
    }

    const proxyUrl = `http://127.0.0.1:${PORT}/proxy/${hash}`;

    ffmpeg.ffprobe(proxyUrl, (err, metadata) => {
        if (err) {
            console.error('[Subtitles] FFprobe error:', err.message);
            return res.status(500).json({ error: 'Failed to probe video' });
        }

        applyProbeMetadata(engine, metadata);
        const subtitleStreams = engine.embeddedSubtitles || [];

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
    res.json(getDownloadsPayload());
});

serverApp.get('/download/events', (req, res) => {
    res.setHeader('Content-Type', 'text/event-stream');
    res.setHeader('Cache-Control', 'no-cache, no-transform');
    res.setHeader('Connection', 'keep-alive');
    res.setHeader('Access-Control-Allow-Origin', '*');
    res.flushHeaders?.();

    // Send initial snapshot immediately
    const initial = `data: ${JSON.stringify(getDownloadsPayload())}\n\n`;
    res.write(initial);

    sseDownloadClients.add(res);

    // Heartbeat ping every 15s to keep connection alive through proxies
    const heartbeat = setInterval(() => {
        try {
            res.write(': keepalive\n\n');
        } catch (e) {
            clearInterval(heartbeat);
            sseDownloadClients.delete(res);
        }
    }, 15000);

    req.on('close', () => {
        clearInterval(heartbeat);
        sseDownloadClients.delete(res);
    });
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
    active.meta.autoPausedForStream = false;
    persistDownloadMeta(downloadId, active.meta);
    activeDownloads.delete(downloadId);
    broadcastDownloadEvents();

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
    broadcastDownloadEvents();
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
        if (active.stalledTimeout) {
            try { clearTimeout(active.stalledTimeout); } catch (e) {}
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
    broadcastDownloadEvents();

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


// Batch and customization routes for Downloads
serverApp.get('/download/pause-all', (req, res) => {
    const allMeta = loadDownloadsMeta();
    let count = 0;
    for (const [dlId, active] of activeDownloads.entries()) {
        if (active.interval) clearInterval(active.interval);
        if (active.stalledTimeout) clearTimeout(active.stalledTimeout);
        if (active.engine) {
            try { active.engine.destroy(); } catch (e) {}
        }
        active.meta.status = 'paused';
        active.meta.autoPausedForStream = false;
        persistDownloadMeta(dlId, active.meta);
        activeDownloads.delete(dlId);
        count++;
    }
    for (const [id, item] of Object.entries(allMeta.downloads)) {
        if (item.status === 'queued') {
            item.status = 'paused';
            count++;
        }
    }
    saveDownloadsMeta(allMeta);
    broadcastDownloadEvents();
    res.json({ status: 'paused_all', count });
});

serverApp.get('/download/resume-all', (req, res) => {
    const allMeta = loadDownloadsMeta();
    let count = 0;
    for (const [id, item] of Object.entries(allMeta.downloads)) {
        if (item.status === 'paused') {
            item.status = 'queued';
            item.queuePosition = Date.now() + count;
            count++;
        }
    }
    saveDownloadsMeta(allMeta);
    pumpDownloadQueue();
    broadcastDownloadEvents();
    res.json({ status: 'resumed_all', count });
});

serverApp.get('/download/clear-completed', (req, res) => {
    const allMeta = loadDownloadsMeta();
    let count = 0;
    for (const [id, item] of Object.entries(allMeta.downloads)) {
        if (item.status === 'completed') {
            delete allMeta.downloads[id];
            count++;
        }
    }
    saveDownloadsMeta(allMeta);
    broadcastDownloadEvents();
    res.json({ status: 'cleared_completed', count });
});

serverApp.get('/download/settings', (req, res) => {
    const allMeta = loadDownloadsMeta();
    const settings = (allMeta && allMeta.settings) || { maxConcurrent: 2, autoPauseOnPlayback: true };
    res.json({
        settings,
        diskSpace: getDiskSpace(),
        downloadDir: DOWNLOADS_DIR
    });
});

serverApp.all('/download/settings/set', (req, res) => {
    const allMeta = loadDownloadsMeta();
    if (!allMeta.settings) allMeta.settings = { maxConcurrent: 2, autoPauseOnPlayback: true };
    const maxConcurrent = req.query.maxConcurrent ?? req.body?.maxConcurrent;
    const autoPauseOnPlayback = req.query.autoPauseOnPlayback ?? req.body?.autoPauseOnPlayback;
    if (maxConcurrent !== undefined) {
        allMeta.settings.maxConcurrent = parseInt(maxConcurrent, 10);
    }
    if (autoPauseOnPlayback !== undefined) {
        allMeta.settings.autoPauseOnPlayback = autoPauseOnPlayback === true || autoPauseOnPlayback === 'true' || autoPauseOnPlayback === 1 || autoPauseOnPlayback === '1';
    }
    saveDownloadsMeta(allMeta);
    pumpDownloadQueue();
    broadcastDownloadEvents();
    res.json({ status: 'ok', settings: allMeta.settings });
});

serverApp.get('/download/force-start/:id', (req, res) => {
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
    if (!saved) return res.status(404).json({ error: 'Download not found' });
    startDownload(saved.hash, saved.title, saved.imdbId, saved.season, saved.episode, saved.fileIdx, true);
    broadcastDownloadEvents();
    res.json({ status: 'started', hash: saved.hash, id: saved.id });
});
