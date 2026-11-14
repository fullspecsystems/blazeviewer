# The FFmpeg version Windows builds against (task #141). One place, read by two scripts:
#   setup-libheif.ps1   patches the pinned vcpkg ffmpeg port to this version, then
#                       rebuilds it whenever the installed copy differs.
#   release-windows.ps1 refuses to package any other installed version, so a box that
#                       pulled a new pin but never re-ran setup cannot ship the old one.
#
# Why not just move -VcpkgRef: upstream vcpkg went from 8.1.2 straight to 9.0.x, a soname
# major (avcodec-62 -> 63) that would also drag libheif / libde265 / dav1d to new versions.
# macOS pins the same version from the release tarball (scripts/build-ffmpeg-macos.sh).
#
# Re-pin checklist (all four were done for 8.1.3 on 2026-11-13):
#   1. The tag commit is the same on GitHub and FFmpeg's own git:
#        git ls-remote https://github.com/FFmpeg/FFmpeg   refs/tags/nX.Y.Z
#        git ls-remote https://git.ffmpeg.org/ffmpeg.git refs/tags/nX.Y.Z
#   2. The release tarball has a good PGP signature from the FFmpeg release signing key
#      FCF9 86EA 15E6 E293 A564 4F10 B432 2F04 D676 58D8 (recipe in build-ffmpeg-macos.sh).
#   3. GitHub's tag archive (what vcpkg downloads) matches that signed tarball file for
#      file. Expected differences: the archive adds .gitignore / .gitattributes, and the
#      tarball adds VERSION. Sha512 below is the hash of
#      https://github.com/ffmpeg/ffmpeg/archive/nX.Y.Z.tar.gz.
#   4. Every patch in the port's ports\ffmpeg\ still applies (setup fails loudly if not).
@{
    Version = '8.1.3'
    Sha512  = '8c667090bcdc70796caaa8cdc2cf82e57d7c7f6feedcb0ec7251cefd6a0c63b7cad203a86c800d3e2f69ca91255167aa4e631701060b96d576d1a9666ca7aafe'
}
