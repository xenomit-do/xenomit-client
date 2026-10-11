param()

$ErrorActionPreference = 'Stop'
$sdk = Join-Path $env:TEMP 'dobot-flex\flex'
$playerGlobal = Join-Path $env:TEMP 'dobot-flex\playerglobal32_0.swc'
$frameworks = Join-Path $sdk 'frameworks'
$source = Join-Path $PSScriptRoot 'DobotBridge.as'
$swf = Join-Path $env:TEMP 'xenomit-DobotBridge.swf'
$abc = Join-Path $PSScriptRoot 'DobotBridge.abc'
if (!(Test-Path (Join-Path $sdk 'lib\mxmlc.jar'))) { throw 'Flex SDK not found' }
if (!(Test-Path $playerGlobal)) { throw 'playerglobal32_0.swc not found' }

$playerDir = Join-Path $frameworks 'libs\player\32.0'
New-Item -ItemType Directory -Force -Path $playerDir | Out-Null
Copy-Item $playerGlobal (Join-Path $playerDir 'playerglobal.swc') -Force
$config = Join-Path $frameworks 'flex-config.xml'
$contents = Get-Content $config -Raw
if ($contents.Contains('{playerglobalHome}')) {
    Set-Content $config $contents.Replace('{playerglobalHome}', 'libs/player') -Encoding ASCII
}

cmd /d /s /c "java -jar `"$sdk\lib\mxmlc.jar`" +flexlib=`"$frameworks`" -target-player=32.0 -warnings=false -output `"$swf`" `"$source`""
if ($LASTEXITCODE -ne 0) { throw "mxmlc failed: $LASTEXITCODE" }
python (Join-Path $PSScriptRoot 'extract_abc.py') $swf $abc
if ($LASTEXITCODE -ne 0) { throw "ABC extraction failed: $LASTEXITCODE" }
