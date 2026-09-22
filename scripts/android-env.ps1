param(
  [string]$JavaHome = $env:JAVA_HOME,
  [string]$AndroidHome = $env:ANDROID_HOME,
  [string]$NdkVersion = "27.2.12479018"
)

if (-not $JavaHome -or -not (Test-Path -LiteralPath "$JavaHome\bin\java.exe")) {
  throw "请通过 -JavaHome 指定 JDK 17，例如 Android Studio 的 jbr 或 HBuilderX 的 amazon-corretto。"
}
if (-not $AndroidHome -or -not (Test-Path -LiteralPath "$AndroidHome\platform-tools")) {
  throw "请通过 -AndroidHome 指定已安装 Platform Tools、Build Tools、Platform 36 和 NDK 的 Android SDK。"
}

$env:JAVA_HOME = $JavaHome
$env:ANDROID_HOME = $AndroidHome
$env:ANDROID_SDK_ROOT = $env:ANDROID_HOME
$env:NDK_HOME = "$AndroidHome\ndk\$NdkVersion"
if (-not $env:CARGO_HOME) { $env:CARGO_HOME = "$env:USERPROFILE\.cargo" }
if (-not $env:RUSTUP_HOME) { $env:RUSTUP_HOME = "$env:USERPROFILE\.rustup" }
$env:PATH = "$env:JAVA_HOME\bin;$env:ANDROID_HOME\platform-tools;$env:CARGO_HOME\bin;$env:PATH"

Write-Host "Android environment ready: $env:ANDROID_HOME"
