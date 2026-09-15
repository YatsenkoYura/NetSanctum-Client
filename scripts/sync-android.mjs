import { access, copyFile, mkdir, readFile, writeFile } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const source = resolve(root, "src-tauri/android/NodeViewPlugin.kt");
const androidProject = resolve(root, "src-tauri/gen/android/app");
const target = resolve(
  androidProject,
  "src/main/java/dev/netsanctum/desktop/NodeViewPlugin.kt",
);
const networkSecuritySource = resolve(root, "src-tauri/android/network_security_config.xml");
const networkSecurityTarget = resolve(androidProject, "src/main/res/xml/network_security_config.xml");
const manifestPath = resolve(androidProject, "src/main/AndroidManifest.xml");

try {
  await access(androidProject);
} catch {
  process.exit(0);
}
await mkdir(dirname(target), { recursive: true });
await copyFile(source, target);
await mkdir(dirname(networkSecurityTarget), { recursive: true });
await copyFile(networkSecuritySource, networkSecurityTarget);

let manifest = await readFile(manifestPath, "utf8");
if (!manifest.includes("android.permission.POST_NOTIFICATIONS")) {
  manifest = manifest.replace(
    "<uses-permission android:name=\"android.permission.INTERNET\" />",
    '<uses-permission android:name="android.permission.INTERNET" />\n    <uses-permission android:name="android.permission.POST_NOTIFICATIONS" />',
  );
}
if (!manifest.includes("android.permission.FOREGROUND_SERVICE_MEDIA_PLAYBACK")) {
  manifest = manifest.replace(
    '<uses-permission android:name="android.permission.POST_NOTIFICATIONS" />',
    '<uses-permission android:name="android.permission.POST_NOTIFICATIONS" />\n    <uses-permission android:name="android.permission.FOREGROUND_SERVICE" />\n    <uses-permission android:name="android.permission.FOREGROUND_SERVICE_MEDIA_PLAYBACK" />\n    <uses-permission android:name="android.permission.WAKE_LOCK" />',
  );
}
if (!manifest.includes('android:name=".MediaPlaybackService"')) {
  manifest = manifest.replace(
    "        <provider",
    '        <receiver\n            android:name=".MediaActionReceiver"\n            android:exported="false" />\n\n        <service\n            android:name=".MediaPlaybackService"\n            android:exported="false"\n            android:foregroundServiceType="mediaPlayback" />\n\n        <provider',
  );
}
if (!manifest.includes("android:networkSecurityConfig")) {
  manifest = manifest.replace(
    'android:usesCleartextTraffic="${usesCleartextTraffic}">',
    'android:usesCleartextTraffic="${usesCleartextTraffic}"\n        android:networkSecurityConfig="@xml/network_security_config">',
  );
}
await writeFile(manifestPath, manifest);
