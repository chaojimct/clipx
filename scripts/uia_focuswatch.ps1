# Focus watcher: sample the focused UIA element for N seconds, print on change.
# Usage: run it, then click into each QQ login box while it samples.
param([int]$Secs = 12)
Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes
$AE = [System.Windows.Automation.AutomationElement]
$deadline = (Get-Date).AddSeconds($Secs)
Write-Output "SAMPLING ${Secs}s - click into QQ account box, then password box now..."
$last = ""
$errs = 0
while ((Get-Date) -lt $deadline) {
    try {
        $el = $AE::FocusedElement
        if ($null -eq $el) {
            if ($last -ne "NULL") { Write-Output "FOCUS=NULL"; $last = "NULL" }
        } else {
            $pid = $el.GetCurrentPropertyValue($AE::ProcessIdProperty)
            try { $pn = (Get-Process -Id $pid -ErrorAction Stop).ProcessName } catch { $pn = "(pid $pid, no lookup)" }
            $ct = $el.GetCurrentPropertyValue($AE::ControlTypeProperty).ProgrammaticName
            $nm = $el.GetCurrentPropertyValue($AE::NameProperty)
            $pw = $el.GetCurrentPropertyValue($AE::IsPasswordProperty)
            try { $null = $el.GetCurrentPattern([System.Windows.Automation.ValuePattern]::Pattern); $vp = "yes" } catch { $vp = "no" }
            try { $r = $el.GetCurrentPropertyValue($AE::BoundingRectangleProperty); $rc = "$([int]$r.X),$([int]$r.Y),$([int]$r.Width)x$([int]$r.Height)" } catch { $rc = "?" }
            $line = "exe=$pn type=$ct name=[$nm] isPassword=$pw valuePattern=$vp rect=$rc"
            if ($line -ne $last) { Write-Output $line; $last = $line }
        }
    } catch {
        $errs++
        if ($errs -le 2) { Write-Output ("ERR: " + $_.Exception.Message) }
    }
    Start-Sleep -Milliseconds 400
}
Write-Output "DONE"
