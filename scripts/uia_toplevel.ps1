# List all top-level windows with pid/exe/class/name (read-only).
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
        if ($pn -match 'QQ|Weixin|WeChat|WXWork|explorer|clipx') {
            Write-Output "pid=$p exe=$pn class=[$c] name=[$n]"
        }
    } catch { }
}
Write-Output "DONE total_top=$($ws.Count)"
