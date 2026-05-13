function Resolve-Symlinks {
    [CmdletBinding()]
    [OutputType([string])]
    param(
        [Parameter(Position = 0, Mandatory, ValueFromPipeline, ValueFromPipelineByPropertyName)]
        [string] $Path
    )

    [string] $separator = '/'
    [string] $normalizedPath = $Path.Replace('\', '/')
    [string[]] $parts = $normalizedPath.Split($separator, [System.StringSplitOptions]::RemoveEmptyEntries)

    [string] $realPath = ''
    foreach ($part in $parts) {
        if ($realPath) {
            if (!$realPath.EndsWith($separator)) {
                $realPath += $separator
            }
            $realPath += $part
        } else {
            $realPath = $part
        }

        $nativePath = $realPath.Replace('/', '\')
        $item = Get-Item -LiteralPath $nativePath -ErrorAction SilentlyContinue
        if ($item -and $item.Target) {
            $realPath = $item.Target.Replace('\', '/')
        }
    }
    $realPath
}

$path=Resolve-Symlinks -Path $args[0]
Write-Host $path
