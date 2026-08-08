$ErrorActionPreference = 'Stop'

$budgets = @{
    'src/app.rs' = 1300
}

$failed = $false
foreach ($entry in $budgets.GetEnumerator()) {
    $path = Join-Path (Join-Path $PSScriptRoot '..') $entry.Key
    $lineCount = @(Get-Content -LiteralPath $path).Count
    $limit = $entry.Value

    if ($lineCount -gt $limit) {
        Write-Error "$($entry.Key) has $lineCount lines; the limit is $limit. Extract the new responsibility before merging."
        $failed = $true
    } else {
        Write-Output "$($entry.Key): $lineCount/$limit lines"
    }
}

if ($failed) {
    exit 1
}
