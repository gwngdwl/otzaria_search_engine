function Resolve-Symlinks {
    [CmdletBinding()]
    [OutputType([string])]
    param(
        [Parameter(Position = 0, Mandatory, ValueFromPipeline, ValueFromPipelineByPropertyName)]
        [string] $Path
    )

    [string] $separator = '/'
    [string] $normalizedPath = $Path.Replace('\', '/')
    [string] $realPath = ''
    [string] $remainingPath = $normalizedPath

    if ($remainingPath.StartsWith('//')) {
        [string[]] $uncParts = $remainingPath.Substring(2).Split($separator, [System.StringSplitOptions]::None)
        if ($uncParts.Length -ge 2) {
            $realPath = "//$($uncParts[0])/$($uncParts[1])"
            $remainingPath = if ($uncParts.Length -gt 2) {
                $uncParts[2..($uncParts.Length - 1)] -join $separator
            } else {
                ''
            }
        }
    } elseif ($remainingPath -match '^[A-Za-z]:') {
        $realPath = $remainingPath.Substring(0, 2)
        $remainingPath = $remainingPath.Substring(2)
        if ($remainingPath.StartsWith($separator)) {
            $realPath += $separator
            $remainingPath = $remainingPath.TrimStart($separator)
        }
    } elseif ($remainingPath.StartsWith($separator)) {
        $realPath = $separator
        $remainingPath = $remainingPath.TrimStart($separator)
    }

    [string[]] $parts = if ($remainingPath) {
        $remainingPath.Split($separator, [System.StringSplitOptions]::RemoveEmptyEntries)
    } else {
        @()
    }

    foreach ($part in $parts) {
        if ($realPath -and !$realPath.EndsWith($separator)) {
            $realPath += $separator
        }
        $realPath += $part

        $nativePath = $realPath.Replace('/', '\')
        $item = Get-Item -LiteralPath $nativePath -ErrorAction SilentlyContinue
        if ($item -and $item.Target) {
            $targetPath = @($item.Target)[0]
            if (-not [System.IO.Path]::IsPathRooted($targetPath)) {
                $targetPath = Join-Path (Split-Path -Parent $nativePath) $targetPath
            }
            $realPath = [System.IO.Path]::GetFullPath($targetPath).Replace('\', '/')
        }
    }

    if (!$realPath) {
        $realPath = $normalizedPath
    }

    $realPath
}

$path=Resolve-Symlinks -Path $args[0]
Write-Host $path
