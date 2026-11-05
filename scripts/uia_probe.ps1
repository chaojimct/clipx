# UIA input-box probe: list editable controls in target process windows.
# Usage: powershell -ExecutionPolicy Bypass -File scripts\uia_probe.ps1 -Exe Weixin
# Read-only: no injection, no system changes.
param(
    [string]$Exe = "Weixin",
    [int]$MaxPerWindow = 40
)
Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes

$procs = Get-Process -Name $Exe -ErrorAction SilentlyContinue
if (-not $procs) { Write-Output "NO_PROC $Exe"; exit 2 }
$pids = @($procs | ForEach-Object { $_.Id } | Sort-Object -Unique)
Write-Output "PROCS $Exe pids=$($pids -join ',')"

$AE = [System.Windows.Automation.AutomationElement]
$condEdit = New-Object System.Windows.Automation.PropertyCondition(
    $AE::ControlTypeProperty, [System.Windows.Automation.ControlType]::Edit)
$condDoc = New-Object System.Windows.Automation.PropertyCondition(
    $AE::ControlTypeProperty, [System.Windows.Automation.ControlType]::Document)
$condCombo = New-Object System.Windows.Automation.PropertyCondition(
    $AE::ControlTypeProperty, [System.Windows.Automation.ControlType]::ComboBox)
$condAny = New-Object System.Windows.Automation.OrCondition($condEdit, $condDoc, $condCombo)

$root = $AE::RootElement
$wins = @($root.FindAll(
    [System.Windows.Automation.TreeScope]::Children,
    [System.Windows.Automation.Condition]::TrueCondition))

$count = 0
foreach ($w in $wins) {
    try { $wpid = $w.GetCurrentPropertyValue($AE::ProcessIdProperty) } catch { continue }
    if ($pids -notcontains $wpid) { continue }
    try { $wname = $w.GetCurrentPropertyValue($AE::NameProperty) } catch { $wname = "?" }
    try { $wcls = $w.GetCurrentPropertyValue($AE::ClassNameProperty) } catch { $wcls = "?" }
    Write-Output "WINDOW pid=$wpid name=[$wname] class=[$wcls]"
    try {
        $edits = @($w.FindAll([System.Windows.Automation.TreeScope]::Descendants, $condAny))
    } catch { continue }
    $n = 0
    foreach ($e in $edits) {
        if ($n -ge $MaxPerWindow) { Write-Output "  ...TRUNCATED total=$($edits.Count)"; break }
        $n++
        try { $ct = $e.GetCurrentPropertyValue($AE::ControlTypeProperty).ProgrammaticName } catch { $ct = "?" }
        try { $nm = $e.GetCurrentPropertyValue($AE::NameProperty) } catch { $nm = "?" }
        try { $pw = $e.GetCurrentPropertyValue($AE::IsPasswordProperty) } catch { $pw = "?" }
        try {
            $null = $e.GetCurrentPattern([System.Windows.Automation.ValuePattern]::Pattern)
            $vp = "yes"
        } catch { $vp = "no" }
        try {
            $r = $e.GetCurrentPropertyValue($AE::BoundingRectangleProperty)
            $rc = "$([int]$r.X),$([int]$r.Y),$([int]$r.Width)x$([int]$r.Height)"
        } catch { $rc = "?" }
        try { $off = $e.GetCurrentPropertyValue($AE::IsOffscreenProperty) } catch { $off = "?" }
        Write-Output "  EDIT type=$ct name=[$nm] isPassword=$pw valuePattern=$vp offscreen=$off rect=$rc"
        $count++
    }
}
Write-Output "TOTAL_EDIT_LIKE=$count"
