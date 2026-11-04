# Asks what OBSPlayed should record for a game it sees for the first time.
# The game name comes from $env:OBSPLAYED_GAME. Prints full / highlights / off,
# or nothing if the window is closed without choosing.
Add-Type -AssemblyName System.Windows.Forms, System.Drawing
[System.Windows.Forms.Application]::EnableVisualStyles()

$bg      = [System.Drawing.Color]::FromArgb(24, 25, 31)
$card    = [System.Drawing.Color]::FromArgb(36, 38, 47)
$hover   = [System.Drawing.Color]::FromArgb(48, 51, 63)
$border  = [System.Drawing.Color]::FromArgb(58, 61, 74)
$accent  = [System.Drawing.Color]::FromArgb(59, 130, 246)
$text    = [System.Drawing.Color]::FromArgb(230, 232, 238)
$muted   = [System.Drawing.Color]::FromArgb(150, 155, 170)

$game = $env:OBSPLAYED_GAME
$form = New-Object System.Windows.Forms.Form
$form.Text = "OBSPlayed"
$form.FormBorderStyle = "FixedSingle"
$form.StartPosition = "CenterScreen"
$form.MaximizeBox = $false
$form.MinimizeBox = $false
$form.TopMost = $true
$form.BackColor = $bg
$form.ForeColor = $text
$form.ClientSize = New-Object System.Drawing.Size(520, 250)
$form.Font = New-Object System.Drawing.Font("Segoe UI", 10)

$kicker = New-Object System.Windows.Forms.Label
$kicker.Text = "NEW GAME DETECTED"
$kicker.Font = New-Object System.Drawing.Font("Segoe UI Semibold", 8.5)
$kicker.ForeColor = $accent
$kicker.AutoSize = $true
$kicker.Location = New-Object System.Drawing.Point(20, 18)
$form.Controls.Add($kicker)

$title = New-Object System.Windows.Forms.Label
$title.Text = $game
$title.Font = New-Object System.Drawing.Font("Segoe UI Semibold", 15)
$title.AutoEllipsis = $true
$title.Size = New-Object System.Drawing.Size(480, 34)
$title.Location = New-Object System.Drawing.Point(18, 38)
$form.Controls.Add($title)

$sub = New-Object System.Windows.Forms.Label
$sub.Text = "What should OBSPlayed record for this game? You can change it later in the OBSPlayed panel."
$sub.ForeColor = $muted
$sub.Size = New-Object System.Drawing.Size(480, 40)
$sub.Location = New-Object System.Drawing.Point(20, 76)
$form.Controls.Add($sub)

$state = @{ choice = "" }
function Add-Choice($label, $hint, $value, $x, $primary) {
    $b = New-Object System.Windows.Forms.Button
    $b.FlatStyle = "Flat"
    $b.FlatAppearance.BorderColor = $(if ($primary) { $accent } else { $border })
    $b.FlatAppearance.MouseOverBackColor = $hover
    $b.BackColor = $card
    $b.ForeColor = $text
    $b.Text = "$label`n$hint"
    $b.TextAlign = "MiddleCenter"
    $b.Size = New-Object System.Drawing.Size(152, 70)
    $b.Location = New-Object System.Drawing.Point($x, 128)
    $b.Cursor = [System.Windows.Forms.Cursors]::Hand
    $b.Add_Click({ $state.choice = $value; $form.Close() }.GetNewClosure())
    $form.Controls.Add($b)
    return $b
}
$full = Add-Choice "Whole session" "record everything" "full" 20 $true
Add-Choice "Only highlights" "clips of kills" "highlights" 184 $false | Out-Null
Add-Choice "Nothing" "ignore this game" "off" 348 $false | Out-Null
$form.AcceptButton = $full

$foot = New-Object System.Windows.Forms.Label
$foot.Text = "Close this window to skip for now (you'll be asked next time)."
$foot.ForeColor = $muted
$foot.Font = New-Object System.Drawing.Font("Segoe UI", 8.5)
$foot.AutoSize = $true
$foot.Location = New-Object System.Drawing.Point(20, 214)
$form.Controls.Add($foot)

try {
    Add-Type -Namespace OBSPlayed -Name Dwm -MemberDefinition '[DllImport("dwmapi.dll")] public static extern int DwmSetWindowAttribute(IntPtr hwnd, int attr, ref int value, int size);'
    $form.Add_HandleCreated({ $dark = 1; [void][OBSPlayed.Dwm]::DwmSetWindowAttribute($form.Handle, 20, [ref]$dark, 4) })
} catch { }
$form.Add_Shown({ $form.Activate() })
if ($env:OBSPLAYED_AUTOCLICK) {
    # Used by the tests: click a button by label right after the window opens.
    $timer = New-Object System.Windows.Forms.Timer
    $timer.Interval = [int]("0" + $env:OBSPLAYED_AUTOCLICK_DELAY) + 300
    $timer.Add_Tick({
        $timer.Stop()
        if ($env:OBSPLAYED_SCREENSHOT) {
            $r = $form.Bounds
            $bmp = New-Object System.Drawing.Bitmap $r.Width, $r.Height
            [System.Drawing.Graphics]::FromImage($bmp).CopyFromScreen($r.Location, [System.Drawing.Point]::Empty, $r.Size)
            $bmp.Save($env:OBSPLAYED_SCREENSHOT)
        }
        foreach ($c in $form.Controls) {
            if ($c -is [System.Windows.Forms.Button] -and $c.Text.StartsWith($env:OBSPLAYED_AUTOCLICK)) { $c.PerformClick() }
        }
    })
    $timer.Start()
}
[void]$form.ShowDialog()
Write-Output $state.choice
