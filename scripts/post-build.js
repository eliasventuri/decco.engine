const fs = require('fs');
const path = require('path');

const distDir = path.join(__dirname, '../dist');
const packageJson = require('../package.json');
const version = packageJson.version;
const appName = "Decco Engine Setup";

const versionedFile = path.join(distDir, `${appName} ${version}.exe`);
const latestFile = path.join(distDir, `${appName} Latest.exe`);

console.log(`[Post-Build] Looking for: ${versionedFile}`);

if (fs.existsSync(versionedFile)) {
    console.log(`[Post-Build] Found versioned installer. Copying to Latest...`);
    fs.copyFileSync(versionedFile, latestFile);
    console.log(`[Post-Build] Successfully created: ${latestFile}`);
} else {
    console.error(`[Post-Build] Error: Could not find versioned installer at ${versionedFile}`);
    // Don't fail the build, just log error, maybe it's a different arch or target
}
