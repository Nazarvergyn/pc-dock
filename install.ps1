# Установщик PC Dock (Windows): скачивает релиз, проверяет SHA256SUMS, ставит бинарники в %USERPROFILE%\.pcdock\bin.
#   irm https://raw.githubusercontent.com/toxic-sunflower/pc-dock/main/install.ps1 | iex
#   .\install.ps1 -Version v0.1.0-alpha.2     без версии берётся самый свежий релиз (в том числе альфа)
param([string]$Version = $env:PCDOCK_VERSION)
$ErrorActionPreference = 'Stop'
$repo = 'toxic-sunflower/pc-dock'
$home_dir = if ($env:PCDOCK_HOME) { $env:PCDOCK_HOME } else { Join-Path $env:USERPROFILE '.pcdock' }

if (-not $Version) {
  $Version = (Invoke-RestMethod "https://api.github.com/repos/$repo/releases?per_page=1")[0].tag_name
}
$name = "pcdock-$Version-windows-x86_64"
$base = "https://github.com/$repo/releases/download/$Version"
$tmp = Join-Path ([IO.Path]::GetTempPath()) ([Guid]::NewGuid())
New-Item -ItemType Directory $tmp | Out-Null
try {
  Write-Host "Скачиваю $name"
  Invoke-WebRequest "$base/$name.zip" -OutFile "$tmp\$name.zip"
  Invoke-WebRequest "$base/SHA256SUMS" -OutFile "$tmp\SHA256SUMS"
  $line = Get-Content "$tmp\SHA256SUMS" | Where-Object { $_ -match " $([regex]::Escape($name)).zip$" } | Select-Object -First 1
  if (-not $line) { throw "в SHA256SUMS нет $name.zip" }
  $want = ($line -split '\s+')[0]
  $have = (Get-FileHash "$tmp\$name.zip" -Algorithm SHA256).Hash
  if ($want -ne $have.ToLower()) { throw 'контрольная сумма не совпала' }

  Expand-Archive "$tmp\$name.zip" -DestinationPath $tmp
  $bin = Join-Path $home_dir 'bin'
  New-Item -ItemType Directory -Force $bin | Out-Null
  foreach ($b in 'pcdockd', 'pcdock-autod', 'pcdock-check', 'pcdock-probe') {
    Copy-Item "$tmp\$name\$b.exe" $bin -Force
  }
  $docs = Join-Path $home_dir 'docs'
  if (Test-Path $docs) { Remove-Item -Recurse -Force $docs }
  Copy-Item "$tmp\$name\docs" $docs -Recurse

  Write-Host "Установлено в $bin (версия $Version)"
  $user_path = [Environment]::GetEnvironmentVariable('Path', 'User')
  if (($user_path -split ';') -notcontains $bin) {
    [Environment]::SetEnvironmentVariable('Path', "$user_path;$bin", 'User')
    Write-Host 'Каталог добавлен в PATH пользователя; откройте новое окно терминала.'
  }
  Write-Host "Проверка телефонов: pcdock-check. Документация: $docs\README.md"
} finally {
  Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}
