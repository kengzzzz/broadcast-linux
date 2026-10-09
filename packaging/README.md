# AUR publishing

`broadcast-linux-bin` installs the GitHub release tarball without compiling.
`packaging/PKGBUILD` remains available for source builds.

Publish the matching GitHub tag and wait for its release tarball before uploading
to AUR. The placeholder checksum must be replaced with that asset's SHA-256.

Use an AUR account with a registered SSH key. From the project root, create a
checkout if one has not already been prepared:

```sh
git clone ssh://aur@aur.archlinux.org/broadcast-linux-bin.git build/aur-bin
cp packaging/aur-bin/PKGBUILD packaging/aur-bin/.SRCINFO packaging/broadcast-linux.install LICENSE build/aur-bin/
cd build/aur-bin
updpkgsums
makepkg --printsrcinfo > .SRCINFO
makepkg --verifysource --force
git add PKGBUILD .SRCINFO broadcast-linux.install LICENSE
git commit -m "Release 0.4.2"
git push origin master
```

`updpkgsums` is provided by `pacman-contrib`. For subsequent releases, pull the AUR
checkout, update `pkgver`, reset `pkgrel=1`, then repeat from `updpkgsums`.
Copy the final `PKGBUILD` and `.SRCINFO` back to `packaging/aur-bin/`.
