param(
    [Parameter(Mandatory=$true)][string]$WorkbookPath,
    [int]$TimeoutSeconds = 600,
    [switch]$DryRun
)
# Exit codes: 0 ok, 2 timeout, 3 excel/COM error. Last stdout line = JSON status.
$ErrorActionPreference = 'Stop'
Add-Type @"
using System;
using System.Runtime.InteropServices;
public class Win32 {
    [DllImport("user32.dll")]
    public static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint pid);
}
"@

$excel = $null; $book = $null; $excelPid = 0
$sw = [System.Diagnostics.Stopwatch]::StartNew()
$exit = 3; $status = 'excel-error'; $detail = ''

function Test-StillRequesting($wb) {
    # Find() relies on Excel's session-persisted LookIn/LookAt UI defaults when those
    # args are omitted; a fresh COM instance may default to searching formulas instead
    # of displayed values, making this loop exit instantly. Pass them explicitly:
    # LookIn=xlValues (-4163), LookAt=xlPart (2), After=[Type]::Missing.
    foreach ($sheet in $wb.Worksheets) {
        $used = $sheet.UsedRange
        if ($null -ne $used) {
            $hit = $used.Find('Requesting Data', [Type]::Missing, -4163, 2)
            if ($null -ne $hit) { return $true }
        }
    }
    return $false
}

try {
    if (-not (Test-Path $WorkbookPath)) { throw "workbook not found: $WorkbookPath" }
    $excel = New-Object -ComObject Excel.Application
    $excel.Visible = $false
    $excel.DisplayAlerts = $false
    [void][Win32]::GetWindowThreadProcessId([IntPtr]$excel.Hwnd, [ref]$excelPid)

    $book = $excel.Workbooks.Open((Resolve-Path $WorkbookPath).Path)

    if (-not $DryRun) {
        # Give the Bloomberg add-in time to load, then force the static refresh.
        Start-Sleep -Seconds 15
        try { $excel.Run('RefreshAllStaticData') } catch { $detail = "RefreshAllStaticData: $_" }

        while (Test-StillRequesting $book) {
            if ($sw.Elapsed.TotalSeconds -gt $TimeoutSeconds) {
                $exit = 2; $status = 'timeout'
                $detail = "still requesting after $TimeoutSeconds s"
                throw 'timeout'
            }
            Start-Sleep -Seconds 5
        }
    }

    $book.Save()
    $exit = 0; $status = 'ok'; $detail = ''
}
catch {
    if ($exit -ne 2) { $exit = 3; $status = 'excel-error'; if (-not $detail) { $detail = "$_" } }
}
finally {
    try { if ($null -ne $book) { $book.Close($false) } } catch {}
    try { if ($null -ne $excel) { $excel.Quit() } } catch {}
    try {
        if ($null -ne $excel) {
            [void][System.Runtime.InteropServices.Marshal]::ReleaseComObject($excel)
        }
    } catch {}
    # Kill the exact Excel process we started if it survived Quit().
    if ($excelPid -gt 0) {
        $p = Get-Process -Id $excelPid -ErrorAction SilentlyContinue
        if ($null -ne $p) { try { $p.Kill() } catch {} }
    }
}

@{ status = $status; seconds = [math]::Round($sw.Elapsed.TotalSeconds, 1); detail = $detail } |
    ConvertTo-Json -Compress
exit $exit
