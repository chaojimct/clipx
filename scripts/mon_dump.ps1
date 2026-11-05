# Dump monitor layout (read-only).
Add-Type -AssemblyName System.Windows.Forms
foreach ($s in [System.Windows.Forms.Screen]::AllScreens) {
    $b = $s.Bounds
    $w = $s.WorkingArea
    Write-Output ("scr=" + $s.DeviceName + " primary=" + $s.Primary + " bounds=" + $b.X + "," + $b.Y + "," + $b.Width + "x" + $b.Height + " work=" + $w.X + "," + $w.Y + "," + $w.Width + "x" + $w.Height)
}
Write-Output "DONE"
