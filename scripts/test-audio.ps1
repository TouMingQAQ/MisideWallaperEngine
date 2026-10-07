param([int]$Seconds = 60)
# 循环播放一个系统自带的 wav，让本进程持续通过 Windows 音频引擎渲染声音。
# 这样它就会出现在音频会话列表里，可以被"进程回环"采集到 —— 用来验证采集链路。
#
# 用法：
#   powershell -NoProfile -ExecutionPolicy Bypass -File scripts/test-audio.ps1 -Seconds 60
# 然后在本应用的窗口列表里选 "powershell.exe" 点开始采集。
$ErrorActionPreference = 'Stop'

$wav = 'C:\Windows\Media\Alarm01.wav'
if (-not (Test-Path $wav)) {
    $wav = (Get-ChildItem 'C:\Windows\Media\*.wav' | Select-Object -First 1).FullName
}

Write-Host "PLAYING pid=$PID wav=$wav"
$player = New-Object System.Media.SoundPlayer $wav
$player.PlayLooping()
Start-Sleep -Seconds $Seconds
$player.Stop()
Write-Host 'DONE'
