# Android 版说明

## 当前状态

Android 版沿用 Vue 3 + Tauri 2 + Rust，不需要改写成 uni-app。已完成 ARM64 APK 构建验证，最低 Android 7（API 24），目标 API 36。

可从 [v1.3.0 Android 预览版](https://github.com/lix965996-art/QzoneArchive-Offline/releases/tag/v1.3.0-android-preview.1) 下载 APK。当前包使用 Android 调试证书签名，仅供侧载测试；后续稳定版会换成独立、长期保存的发行证书。

- 支持二维码登录、归档任务、记录浏览、图片/视频查看和本地 SQLite 存储。
- 手机端使用应用专属数据目录，避免 Android 文档目录 URI 被误当成本地路径而损坏数据库。
- 媒体批量下载先保存到应用导出目录。
- ZIP 先在应用目录可靠生成，再以分块方式复制到系统文件选择器指定的位置，避免把大 ZIP 一次性读入内存。
- Android 没有桌面 Edge 可执行文件，因此手机端不生成 PDF。需要 PDF 时请使用 Windows 版；手机 ZIP 仍包含可离线阅读的 HTML 和已下载媒体。

## 本机开发环境

HBuilderX 不是本项目的构建工具，但它附带的 Amazon Corretto JDK 17 和 ADB 可以复用。仍需另装 Android SDK Platform、Platform Tools、Build Tools 与 NDK。

```powershell
.\scripts\android-env.ps1 `
  -JavaHome "D:\path\to\HBuilderX\plugins\amazon-corretto" `
  -AndroidHome "F:\DevCache\AndroidSdk"

npm ci
npm run tauri:android:init -- --ci
npm run tauri -- android build --debug --apk --target aarch64 --ci
```

若 Windows 未启用开发者模式，Tauri CLI 可能因无权创建符号链接而中止。可以启用 Windows 开发者模式后重试；不要为此关闭系统安全防护或使用来源不明的提权脚本。

## 已知限制

- 当前公开预览包只验证了构建、APK 签名和静态清单；在发布稳定版前仍需一台真实 Android 手机验证二维码登录、长任务后台行为、系统文件选择器和大 ZIP 复制。
- Android 系统可能在锁屏或省电模式下限制长时间网络任务，归档时建议保持应用在前台。
- 系统 WebView 版本会影响网页登录和媒体播放兼容性，建议保持 Android System WebView 更新。
- 手机 ZIP 不含 PDF；PDF 是桌面端功能。
