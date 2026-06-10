# sigil-tray.ps1 — Windows system-tray helper for the sigil-top terminal node.
# Spawned by sigil-top (spawn_system_tray); runs hidden. Provides a notification-area icon with a
# menu: Open Wallet / Open Block Explorer / Start at login (toggle) / Quit. ISOLATED from the node
# — a failure here can never affect sigil-top. Auto-exits when the node process (NodePid) dies.
param([int]$NodePid = 0, [string]$ExePath = "")
$ErrorActionPreference = "SilentlyContinue"
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing

$wallet   = "http://localhost:9800/"
$explorer = "http://localhost:9800/sigil-explorer.html"
$runKey   = "HKCU:\Software\Microsoft\Windows\CurrentVersion\Run"

$ni = New-Object System.Windows.Forms.NotifyIcon
$ni.Icon = [System.Drawing.SystemIcons]::Information
$ni.Text = "SIGIL node — sigil-top"
$ni.Visible = $true

$menu = New-Object System.Windows.Forms.ContextMenuStrip

$mWallet = New-Object System.Windows.Forms.ToolStripMenuItem("Open Wallet")
$mWallet.add_Click({ Start-Process $wallet })
[void]$menu.Items.Add($mWallet)

$mExplorer = New-Object System.Windows.Forms.ToolStripMenuItem("Open Block Explorer")
$mExplorer.add_Click({ Start-Process $explorer })
[void]$menu.Items.Add($mExplorer)

[void]$menu.Items.Add((New-Object System.Windows.Forms.ToolStripSeparator))

$mStart = New-Object System.Windows.Forms.ToolStripMenuItem("Start at login")
$mStart.CheckOnClick = $true
$mStart.Checked = ($null -ne (Get-ItemProperty -Path $runKey -Name "SigilTop" -ErrorAction SilentlyContinue))
$mStart.add_Click({
  if ($mStart.Checked) {
    if ($ExePath -ne "") { Set-ItemProperty -Path $runKey -Name "SigilTop" -Value ('"' + $ExePath + '"') }
  } else {
    Remove-ItemProperty -Path $runKey -Name "SigilTop" -ErrorAction SilentlyContinue
  }
})
[void]$menu.Items.Add($mStart)

[void]$menu.Items.Add((New-Object System.Windows.Forms.ToolStripSeparator))

$mQuit = New-Object System.Windows.Forms.ToolStripMenuItem("Quit SIGIL node")
$mQuit.add_Click({
  if ($NodePid -gt 0) { Stop-Process -Id $NodePid -Force -ErrorAction SilentlyContinue }
  $ni.Visible = $false; $ni.Dispose(); [System.Windows.Forms.Application]::Exit()
})
[void]$menu.Items.Add($mQuit)

$ni.ContextMenuStrip = $menu
$ni.add_MouseDoubleClick({ Start-Process $wallet })
$ni.ShowBalloonTip(2500, "SIGIL node running", "Right-click the tray icon for wallet, explorer, and start-at-login.", [System.Windows.Forms.ToolTipIcon]::Info)

# Mirror the node's lifetime: when sigil-top exits, the tray icon goes away too.
$timer = New-Object System.Windows.Forms.Timer
$timer.Interval = 2000
$timer.add_Tick({
  if ($NodePid -gt 0 -and -not (Get-Process -Id $NodePid -ErrorAction SilentlyContinue)) {
    $ni.Visible = $false; $ni.Dispose(); $timer.Stop(); [System.Windows.Forms.Application]::Exit()
  }
})
$timer.Start()

[System.Windows.Forms.Application]::Run()
