$ErrorActionPreference = 'Stop'
$dir = $PSScriptRoot
$root = (Resolve-Path (Join-Path $dir '../../../../../..')).Path
$compose = Join-Path $root 'testing/docker/compose.yaml'
$db = 'spelaion_plan48_' + [guid]::NewGuid().ToString('N')
$created = $false
function Invoke-Sql([string]$database, [string]$sql) {
    $output = $sql | docker compose -f $compose exec -T sqlserver /bin/bash -c 'export SQLCMDPASSWORD="$MSSQL_SA_PASSWORD"; exec /opt/mssql-tools18/bin/sqlcmd -C -S localhost -U sa -b -y 0 -w 65535 -d "$1"' _ $database
    if ($LASTEXITCODE -ne 0) { throw "sqlcmd failed for $database`: $($output -join ' ')" }
    return ($output -join "`n")
}
try {
    Invoke-Sql 'master' "CREATE DATABASE [$db];" | Out-Null
    $created = $true
    Invoke-Sql $db (Get-Content "$dir/setup.sql" -Raw) | Out-Null
    foreach ($name in @('seek', 'scan', 'join', 'parallel', 'spill')) {
        $mode = if ($name -in @('parallel', 'spill')) { 'STATISTICS XML' } else { 'SHOWPLAN_XML' }
        $query = Get-Content "$dir/$name.sql" -Raw
        $sql = "SET NOCOUNT ON;`nGO`nSET $mode ON;`nGO`n$query`nGO`nSET $mode OFF;`nGO`n"
        $output = Invoke-Sql $db $sql
        $match = [regex]::Match($output, '(?s)<ShowPlanXML\b.*?</ShowPlanXML>')
        if (!$match.Success) { throw "No ShowPlanXML for $name" }
        [IO.File]::WriteAllText((Join-Path $dir "$name.sqlplan"), $match.Value, [Text.UTF8Encoding]::new($false))
        Write-Output "$name $($match.Value.Length) characters"
    }
    Invoke-Sql 'master' 'SELECT @@VERSION;' | Write-Output
} finally {
    if ($created -and $db -match '^spelaion_plan48_[0-9a-f]{32}$') {
        Invoke-Sql 'master' "ALTER DATABASE [$db] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [$db];" | Out-Null
    }
}
