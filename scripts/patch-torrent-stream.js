const fs = require('fs');
const path = require('path');

function patchFile(filePath, replacements) {
    if (!fs.existsSync(filePath)) return;
    let content = fs.readFileSync(filePath, 'utf8');
    let modified = false;
    for (const [target, replacement] of replacements) {
        if (content.includes(target)) {
            content = content.replace(target, replacement);
            modified = true;
        }
    }
    if (modified) {
        fs.writeFileSync(filePath, content, 'utf8');
        console.log(`[Patch] Applied torrent-stream optimization to ${path.relative(process.cwd(), filePath)}`);
    }
}

const tsIndex = path.join(__dirname, '../node_modules/torrent-stream/index.js');
const tsFileStream = path.join(__dirname, '../node_modules/torrent-stream/lib/file-stream.js');

patchFile(tsIndex, [
    [
        'var MAX_REQUESTS = 5\nvar CHOKE_TIMEOUT = 5000\nvar REQUEST_TIMEOUT = 30000',
        'var MAX_REQUESTS = 24\nvar CHOKE_TIMEOUT = 5000\nvar REQUEST_TIMEOUT = 12000'
    ],
    [
        'pieces[index].cancel((req.offset / piece.BLOCK_SIZE) | 0)',
        'pieces[index].cancel((req.offset / piece.BLOCK_LENGTH) | 0)'
    ],
    [
        '      for (var i = engine.selection.length - 1; i >= 0; i--) {\n        var next = engine.selection[i]\n        for (var j = next.to; j >= next.from + next.offset; j--) {',
        '      for (var i = 0; i < engine.selection.length; i++) {\n        var next = engine.selection[i]\n        for (var j = next.from + next.offset; j <= next.to; j++) {'
    ],
    [
        '    var loop = function (i) {\n      if (i >= torrent.pieces.length) return onready()\n      engine.store.get(i, function (_, buf) {',
        '    var loop = function (i) {\n      if (i >= torrent.pieces.length) return onready()\n      var hasAnyExistingFile = torrent.files && torrent.files.some(function (file) {\n        try {\n          return fs.existsSync(path.join(opts.path, file.path))\n        } catch (_) {\n          return false\n        }\n      })\n      if (!hasAnyExistingFile) return onready()\n      engine.store.get(i, function (_, buf) {'
    ]
]);

patchFile(tsFileStream, [
    [
        '  this._critical = Math.min(1024 * 1024 / pieceLength, 2) | 0',
        '  this._critical = Math.max(1, Math.min(Math.ceil((16 * 1024 * 1024) / pieceLength), 8)) | 0'
    ]
]);

