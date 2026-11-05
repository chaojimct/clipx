# Dump every top-level window: pid/exe/class/title/rect (read-only).
Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes
$AE = [System.Windows.Automation.AutomationElement]
$root = $AE::RootElement
$ws = @($root.FindAll(
    [System.Windows.Automation.TreeScope]::Children,
    [System.Windows.Automation.Condition]::TrueCondition))
foreach ($w in $ws) {
    try {
        $p = $w.GetCurrentPropertyValue($AE::ProcessIdProperty)
        $n = $w.GetCurrentPropertyValue($AE::NameProperty)
        $c = $w.GetCurrentPropertyValue($AE::ClassNameProperty)
        $pn = (Get-Process -Id $p -ErrorAction SilentlyContinue).ProcessName
        if ($pn -match 'QQ|Weixin|WeChat|WXWork') {
            try { $r = $w.GetCurrentPropertyValue($AE::BoundingRectangleProperty); $rc = "$([int]$r.X),$([int]$r.Y),$([int]$r.Width)x$([int]$r.Height)" } catch { $rc = "?" }
            Write-Output "pid=$p exe=$pn class=[$c] name=[$n] rect=$rc"
        }
    } catch { }
}
Write-Output "DONE"
