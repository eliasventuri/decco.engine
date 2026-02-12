const { execSync } = require('child_process');
const path = require('path');
const fs = require('fs');

const GH_PATH_DEFAULT = 'gh';
const GH_PATH_FALLBACK = 'C:\\Program Files\\GitHub CLI\\gh.exe';
let GH_CMD = GH_PATH_DEFAULT;

try {
    // 1. Check if gh CLI is installed
    try {
        execSync(`${GH_CMD} --version`, { stdio: 'ignore' });
    } catch (e) {
        if (fs.existsSync(GH_PATH_FALLBACK)) {
            console.log(`[Release] 'gh' not in PATH, using fallback: ${GH_PATH_FALLBACK}`);
            GH_CMD = `"${GH_PATH_FALLBACK}"`;
        } else {
            console.error('Error: GitHub CLI (gh) is not installed or not in PATH.');
            console.error(`Checked PATH and ${GH_PATH_FALLBACK}`);
            console.error('Please install it: https://cli.github.com/');
            process.exit(1);
        }
    }

    // 2. Check if logged in
    try {
        execSync(`${GH_CMD} auth status`, { stdio: 'ignore' });
    } catch (e) {
        console.error('Error: You are not logged in to GitHub CLI.');
        console.error(`Please run: ${GH_CMD} auth login`);
        process.exit(1);
    }

    // 3. Increment Version
    console.log('[Release] Incrementing version...');
    execSync('npm version patch --no-git-tag-version', { stdio: 'inherit' });

    const packageJson = require('../package.json');
    const version = packageJson.version;
    const tagName = `v${version}`;

    // Commit the version bump
    console.log(`[Release] Committing version ${version}...`);
    execSync('git add package.json package-lock.json', { stdio: 'inherit' });
    execSync(`git commit -m "Release ${tagName}"`, { stdio: 'inherit' });

    // Tag locally
    execSync(`git tag -a ${tagName} -m "Release ${version}"`, { stdio: 'inherit' });

    // 4. Build
    console.log(`[Release] Building version ${version}...`);
    execSync('npm run build', { stdio: 'inherit' });

    // 5. Push Code
    console.log('[Release] Pushing code to GitHub...');
    execSync('git push origin main', { stdio: 'inherit' });
    execSync(`git push origin ${tagName}`, { stdio: 'inherit' });

    // 6. Create Release & Upload Artifacts
    console.log(`[Release] Creating GitHub Release ${tagName}...`);

    const distDir = path.join(__dirname, '../dist');
    // Artifact name matches package.json artifactName: ${name}-Setup-${version}.${ext}
    const installerName = `decco-engine-Setup-${version}.exe`;
    const latestName = `decco-engine-latest.exe`;
    const blockmapName = `${installerName}.blockmap`;
    const ymlName = 'latest.yml';

    const filesToUpload = [
        path.join(distDir, installerName),
        path.join(distDir, latestName),
        path.join(distDir, blockmapName),
        path.join(distDir, ymlName)
    ].filter(f => fs.existsSync(f));

    const fileArgs = filesToUpload.map(f => `"${f}"`).join(' ');

    const cmd = `${GH_CMD} release create ${tagName} ${fileArgs} --title "Release ${tagName}" --notes "Automated release of version ${version}"`;

    execSync(cmd, { stdio: 'inherit' });

    console.log('[Release] Done! Release published successfully.');

} catch (error) {
    console.error('[Release] Failed:', error.message);
    process.exit(1);
}
