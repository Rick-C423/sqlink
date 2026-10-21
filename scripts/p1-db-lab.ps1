param(
    [ValidateSet('StartMySql', 'StartPostgres', 'Stop', 'Status', 'ExportEnv', 'ExportManifest')]
    [string]$Action = 'Status',
    [Parameter(Mandatory = $true)][string]$LabRoot,
    [string]$MySqlBin = 'C:\Program Files\MySQL\MySQL Server 5.7\bin',
    [ValidateSet('mysql', 'mysql57', 'mysql8')][string]$MySqlTag = 'mysql',
    [string]$PostgresBin,
    [string]$ManifestPath
)

# 所有实例均为工作区内独立、可撤销的验收环境；不会读取应用连接配置或安装服务。
$ErrorActionPreference = 'Stop'
$projectRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$allowedRoot = [IO.Path]::GetFullPath((Join-Path $projectRoot '.zcode')) + [IO.Path]::DirectorySeparatorChar
$LabRoot = [IO.Path]::GetFullPath($LabRoot)
if (-not $LabRoot.StartsWith($allowedRoot, [StringComparison]::OrdinalIgnoreCase) -or
    [IO.Path]::GetFileName($LabRoot) -notmatch '^p1-db-lab-[a-f0-9]{32}$') {
    throw '测试目录必须为本项目 .zcode/p1-db-lab-GUID。'
}
New-Item -ItemType Directory -Path $LabRoot -Force | Out-Null
# 禁止凭据文件继承普通用户读取权限，仅当前用户与 SYSTEM 可读写。
$acl = Get-Acl -LiteralPath $LabRoot
$acl.SetAccessRuleProtection($true, $false)
$identity = [Security.Principal.WindowsIdentity]::GetCurrent().Name
foreach ($account in @($identity, 'NT AUTHORITY\SYSTEM')) {
    $rule = [Security.AccessControl.FileSystemAccessRule]::new($account, 'FullControl', 'ContainerInherit,ObjectInherit', 'None', 'Allow')
    $acl.AddAccessRule($rule)
}
Set-Acl -LiteralPath $LabRoot -AclObject $acl
$statePath = Join-Path $LabRoot 'lab-state.json'
$envPath = Join-Path $LabRoot 'test-env.ps1'
$state = if (Test-Path -LiteralPath $statePath) { Get-Content -LiteralPath $statePath -Raw | ConvertFrom-Json -AsHashtable } else { @{} }
function Save-State {
    $state | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $statePath -Encoding utf8
    $lines = foreach ($item in $state.Values) {
        if ($item.Variable -and $item.Url) { '$env:' + $item.Variable + " = '" + $item.Url.Replace("'", "''") + "'" }
    }
    Set-Content -LiteralPath $envPath -Value $lines -Encoding utf8
    foreach ($key in @('mysql', 'mysql57', 'mysql8')) {
        if ($state.ContainsKey($key)) {
            (@($lines) + @('$env:SQLINK_TEST_MYSQL = ''' + $state[$key].Url + "'")) | Set-Content -LiteralPath (Join-Path $LabRoot ($key + '-env.ps1')) -Encoding utf8
        }
    }
    $primary = if ($state.ContainsKey('mysql8')) { 'mysql8' } elseif ($state.ContainsKey('mysql57')) { 'mysql57' } elseif ($state.ContainsKey('mysql')) { 'mysql' }
    if ($primary) { Add-Content -LiteralPath $envPath -Value ('$env:SQLINK_TEST_MYSQL = ''' + $state[$primary].Url + "'") -Encoding utf8 }
}
function Get-FreePort {
    $listener = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback, 0)
    $listener.Start()
    $selectedPort = $listener.LocalEndpoint.Port
    $listener.Stop()
    return $selectedPort
}
function Wait-Loopback([int]$Port, [Diagnostics.Process]$Process) {
    for ($i = 0; $i -lt 120; $i++) {
        $Process.Refresh()
        if ($Process.HasExited) { throw '独立数据库启动失败，请检查专属目录内日志。' }
        $client = [Net.Sockets.TcpClient]::new()
        try { $client.Connect('127.0.0.1', $Port); return } catch {} finally { $client.Dispose() }
        Start-Sleep -Milliseconds 250
    }
    throw '独立数据库等待端口超时。'
}
if ($Action -eq 'StartMySql') {
    if ($state.ContainsKey($MySqlTag)) { throw '此目录已有对应 MySQL 状态；请使用新 GUID 目录或不同标签。' }
    $exe = Join-Path $MySqlBin 'mysqld.exe'
    $version = (& $exe --version | Out-String).Trim()
    $dataDir = Join-Path $LabRoot ($MySqlTag + '-data')
    if (Test-Path -LiteralPath $dataDir) { throw '拒绝覆盖已有数据目录。' }
    $port = Get-FreePort
    $password = [guid]::NewGuid().ToString('N') + [guid]::NewGuid().ToString('N')
    $database = 'sqlink_test_' + [guid]::NewGuid().ToString('N')
    $baseDir = Split-Path -Parent $MySqlBin
    $init = Start-Process -FilePath $exe -ArgumentList @('--no-defaults', '--initialize-insecure', ('--basedir="' + $baseDir + '"'), ('--datadir="' + $dataDir + '"')) -WindowStyle Hidden -Wait -PassThru -RedirectStandardOutput (Join-Path $LabRoot ($MySqlTag + '-init.out.log')) -RedirectStandardError (Join-Path $LabRoot ($MySqlTag + '-init.err.log'))
    if ($init.ExitCode -ne 0) { throw 'MySQL 独立数据目录初始化失败。' }
    $bootstrap = Join-Path $LabRoot ($MySqlTag + '-bootstrap.sql')
    @("ALTER USER 'root'@'localhost' IDENTIFIED BY '$password';", "CREATE USER 'sqlink_test'@'127.0.0.1' IDENTIFIED BY '$password';", "GRANT ALL PRIVILEGES ON *.* TO 'sqlink_test'@'127.0.0.1' WITH GRANT OPTION;", "CREATE DATABASE $database CHARACTER SET utf8mb4;") | Set-Content -LiteralPath $bootstrap -Encoding ascii
    $config = Join-Path $LabRoot ($MySqlTag + '-test.ini')
    @('[mysqld]', ('basedir=' + $baseDir.Replace('\','/')), ('datadir=' + $dataDir.Replace('\','/')), 'bind-address=127.0.0.1', ('port=' + $port), 'skip-name-resolve', 'mysqlx=0', ('init-file=' + $bootstrap.Replace('\','/')), ('pid-file=' + (Join-Path $LabRoot ($MySqlTag + '.pid')).Replace('\','/')), ('log-error=' + (Join-Path $LabRoot ($MySqlTag + '.err.log')).Replace('\','/'))) | Set-Content -LiteralPath $config -Encoding ascii
    # MySQL 5.7 未安装 X Plugin 时不识别 mysqlx，使用 loose 前缀以跨版本关闭。
    (Get-Content -LiteralPath $config) -replace '^mysqlx=0$', 'loose-mysqlx=0' | Set-Content -LiteralPath $config -Encoding ascii
    $serverArguments = @(('--defaults-file="' + $config + '"'))
    # 8.x 在 Windows 默认派生监控进程；命令行选项在监控分派前生效。
    if ($version -match '\bVer (?:8|9)\.') { $serverArguments += '--no-monitor' }
    $server = Start-Process -FilePath $exe -ArgumentList $serverArguments -WindowStyle Hidden -PassThru
    $variable = switch ($MySqlTag) { 'mysql57' { 'SQLINK_TEST_MYSQL57' } 'mysql8' { 'SQLINK_TEST_MYSQL8' } default { 'SQLINK_TEST_MYSQL' } }
    $state[$MySqlTag] = @{ Pid=$server.Id; StartedUtc=$server.StartTime.ToUniversalTime().ToString('o'); DataDir=$dataDir; Bin=$MySqlBin; Port=$port; Variable=$variable; Url="mysql://sqlink_test:${password}@127.0.0.1:$port/$database"; Version=$version }
    Save-State
    Wait-Loopback $port $server
    $actualPid = [int](Get-Content -LiteralPath (Join-Path $LabRoot ($MySqlTag + '.pid')) -First 1)
    $actualProcess = Get-Process -Id $actualPid
    if ($actualProcess.Path -ne $exe) { throw 'MySQL pid-file 与专属二进制路径不符。' }
    $state[$MySqlTag].LauncherPid = $server.Id
    $state[$MySqlTag].Pid = $actualPid
    $state[$MySqlTag].StartedUtc = $actualProcess.StartTime.ToUniversalTime().ToString('o')
    Save-State
    $clientConfig = Join-Path $LabRoot ($MySqlTag + '-client-check.ini')
    @('[client]', 'user=sqlink_test', ('password=' + $password), 'host=127.0.0.1', ('port=' + $port), 'protocol=tcp') | Set-Content -LiteralPath $clientConfig -Encoding ascii
    try {
        $probeOutput = & (Join-Path $MySqlBin 'mysql.exe') ('--defaults-file=' + $clientConfig) ('--database=' + $database) --connect-timeout=3 --batch --execute='SELECT VERSION();' 2>&1
        if ($LASTEXITCODE -ne 0) { throw 'MySQL 专用测试用户实际TCP登录验证失败，详查本轮日志。' }
    } finally { Remove-Item -LiteralPath $clientConfig -ErrorAction SilentlyContinue }
    Remove-Item -LiteralPath $bootstrap
    # 后续重启不再读取已删除的引导 SQL。
    (Get-Content -LiteralPath $config) | Where-Object { $_ -notmatch '^init-file=' } | Set-Content -LiteralPath $config -Encoding ascii
}
if ($Action -eq 'StartPostgres') {
    if ($state.ContainsKey('postgres')) { throw '此目录已有 PostgreSQL 状态；请使用新 GUID 目录。' }
    if (-not $PostgresBin) { throw '必须显式指定已核对来源的 PostgreSQL bin 目录。' }
    $dataDir = Join-Path $LabRoot 'postgres-data'
    if (Test-Path -LiteralPath $dataDir) { throw '拒绝覆盖已有数据目录。' }
    $port = Get-FreePort
    $password = [guid]::NewGuid().ToString('N') + [guid]::NewGuid().ToString('N')
    $database = 'sqlink_test_' + [guid]::NewGuid().ToString('N')
    $pwFile = Join-Path $LabRoot 'postgres-bootstrap-password.txt'
    Set-Content -LiteralPath $pwFile -Value $password -Encoding ascii
    & (Join-Path $PostgresBin 'initdb.exe') -D $dataDir -U sqlink_test_admin -E UTF8 --locale=C --auth=scram-sha-256 --pwfile=$pwFile > (Join-Path $LabRoot 'postgres-init.log') 2>&1
    if ($LASTEXITCODE -ne 0) { throw 'PostgreSQL 独立数据目录初始化失败。' }
    Remove-Item -LiteralPath $pwFile
    @("listen_addresses = '127.0.0.1'", "port = $port", 'max_connections = 40', 'shared_buffers = 32MB') | Add-Content -LiteralPath (Join-Path $dataDir 'postgresql.conf') -Encoding ascii
    $exe = Join-Path $PostgresBin 'postgres.exe'
    # pg_ctl 在 Windows 使用受限令牌，避免以管理员权限直接运行 postgres。
    $starter = Start-Process -FilePath (Join-Path $PostgresBin 'pg_ctl.exe') -ArgumentList @('-D', ('"' + $dataDir + '"'), '-l', ('"' + (Join-Path $LabRoot 'postgres-pgctl.log') + '"'), '-w', 'start') -WindowStyle Hidden -PassThru -RedirectStandardOutput (Join-Path $LabRoot 'postgres-start.out.log') -RedirectStandardError (Join-Path $LabRoot 'postgres-start.err.log')
    # Start-Process -Wait 会等待整个子进程树，包含长期运行的服务器；这里只等 pg_ctl。
    if (-not $starter.WaitForExit(30000)) { throw 'PostgreSQL pg_ctl 启动超时。' }
    if ($starter.ExitCode -ne 0) { throw 'PostgreSQL pg_ctl 启动失败。' }
    $serverPid = [int](Get-Content -LiteralPath (Join-Path $dataDir 'postmaster.pid') -First 1)
    $server = Get-Process -Id $serverPid
    $state.postgres = @{ Pid=$serverPid; StartedUtc=$server.StartTime.ToUniversalTime().ToString('o'); DataDir=$dataDir; Bin=$PostgresBin; Port=$port; Variable='SQLINK_TEST_POSTGRES'; Url="postgres://sqlink_test_admin:${password}@127.0.0.1:$port/$database"; Version=(& $exe --version | Out-String).Trim() }
    Save-State
    Wait-Loopback $port $server
    $previousPassword = $env:PGPASSWORD
    try {
        $env:PGPASSWORD = $password
        & (Join-Path $PostgresBin 'createdb.exe') -h 127.0.0.1 -p $port -U sqlink_test_admin $database
        if ($LASTEXITCODE -ne 0) { throw 'PostgreSQL 独立测试库创建失败。' }
    } finally { $env:PGPASSWORD = $previousPassword }
}
if ($Action -eq 'Stop') {
    foreach ($key in @($state.Keys)) {
        $item = $state[$key]
        $expectedData = [IO.Path]::GetFullPath($item.DataDir)
        if (-not $expectedData.StartsWith($LabRoot + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) { throw '状态中的数据目录超出本轮范围，拒绝停止。' }
        $process = Get-Process -Id $item.Pid -ErrorAction SilentlyContinue
        if (-not $process) { continue }
        $expectedExe = Join-Path $item.Bin $(if ($key -like 'mysql*') { 'mysqld.exe' } else { 'postgres.exe' })
        if ($process.Path -ne $expectedExe) { throw 'PID 已被其他进程复用，拒绝停止。' }
        if (-not $item.StartedUtc -or $process.StartTime.ToUniversalTime() -ne [datetime]$item.StartedUtc) { throw '进程创建时间与记录不符，拒绝停止。' }
        if ($key -eq 'postgres') {
            & (Join-Path $item.Bin 'pg_ctl.exe') -D $expectedData -m fast -w stop
            if ($LASTEXITCODE -ne 0) { throw 'PostgreSQL 停止失败。' }
        } else {
            $url = [uri]$item.Url
            $credential = Join-Path $LabRoot 'mysql-stop.ini'
            @('[client]', ('user=' + $url.UserInfo.Split(':',2)[0]), ('password=' + $url.UserInfo.Split(':',2)[1]), 'host=127.0.0.1', ('port=' + $item.Port), 'protocol=tcp') | Set-Content -LiteralPath $credential -Encoding ascii
            try {
                & (Join-Path $item.Bin 'mysqladmin.exe') ('--defaults-file=' + $credential) shutdown
                if ($LASTEXITCODE -ne 0) { throw 'MySQL 停止失败。' }
            } finally { Remove-Item -LiteralPath $credential -ErrorAction SilentlyContinue }
        }
        if (-not $process.WaitForExit(30000)) { throw '停止指令已发送，但本轮数据进程未在30秒内退出。' }
    }
}
if ($Action -eq 'ExportEnv') { Save-State }
if ($Action -eq 'ExportManifest') {
    if (-not $ManifestPath) { $ManifestPath = Join-Path $projectRoot 'docs/p1-db-lab-manifest.json' }
    $ManifestPath = [IO.Path]::GetFullPath($ManifestPath)
    if (-not $ManifestPath.StartsWith($projectRoot + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) { throw '公开清单必须写入本项目目录。' }
    $assetsPath = Join-Path $LabRoot 'lab-assets.json'
    $assets = if (Test-Path -LiteralPath $assetsPath) { Get-Content -LiteralPath $assetsPath -Raw | ConvertFrom-Json } else { @() }
    $instances = foreach ($key in $state.Keys) {
        $item = $state[$key]
        $process = Get-Process -Id $item.Pid -ErrorAction SilentlyContinue
        $running = [bool]($process -and $item.StartedUtc -and $process.StartTime.ToUniversalTime() -eq [datetime]$item.StartedUtc)
        [ordered]@{ Engine=$key; Version=$item.Version; Host='127.0.0.1'; Port=$item.Port; Pid=$item.Pid; StartedUtc=$item.StartedUtc; Running=$running; DataDir=$item.DataDir; Bin=$item.Bin; Database=([uri]$item.Url).AbsolutePath.Trim('/'); ProtectedEnvFile=$envPath }
    }
    $public = [ordered]@{
        GeneratedUtc=[DateTime]::UtcNow.ToString('o')
        Purpose='本轮真实数据库验收专用实例；无系统服务安装，不改既有数据库。'
        LabRoot=$LabRoot
        Credentials='连接串与密码仅保存于实验目录的 ACL 受限 ignored 文件，公开清单不含凭据。'
        Instances=@($instances)
        DownloadEvidence=@($assets)
        SqlServer='已核对 PATH、已知 Program Files 目录和服务名称，未发现 SQL Server/LocalDB/Express/Docker/Podman 可用运行时；未安装 SQL Server 或创建系统服务。'
    }
    New-Item -ItemType Directory -Path (Split-Path -Parent $ManifestPath) -Force | Out-Null
    $public | ConvertTo-Json -Depth 12 | Set-Content -LiteralPath $ManifestPath -Encoding utf8
    Write-Output ('公开清单：' + $ManifestPath)
}
# 仅输出不含密码的核对信息；连接凭据保存在 ignored 且 ACL 受限目录。
foreach ($key in $state.Keys) {
    $item = $state[$key]
    [pscustomobject]@{ Engine=$key; Version=$item.Version; Port=$item.Port; Pid=$item.Pid; DataDir=$item.DataDir; EnvFile=$envPath }
}
