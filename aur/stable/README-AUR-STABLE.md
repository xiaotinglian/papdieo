# Stable AUR package (papdieo)

This package expects an upstream tag named `v<version>` (example: `v0.1.0`).

## 1) Create and push a release tag upstream

```bash
cd /home/shawn/papdieo
git tag -a v0.1.0 -m "v0.1.0"
git push origin v0.1.0
```

## 2) Generate `.SRCINFO`

```bash
cd /home/shawn/papdieo/aur/stable
makepkg --printsrcinfo > .SRCINFO
```

## 3) Publish `papdieo` on AUR

```bash
git clone ssh://aur@aur.archlinux.org/papdieo.git
cd papdieo
cp /home/shawn/papdieo/aur/stable/PKGBUILD .
cp /home/shawn/papdieo/aur/stable/.SRCINFO .
git add PKGBUILD .SRCINFO
git commit -m "Initial stable release of papdieo"
git push
```

## 4) Update stable release

- Bump `pkgver` in `PKGBUILD` to the new release version.
- Ensure matching upstream tag `v<pkgver>` exists.
- Regenerate `.SRCINFO` and push to AUR.

```bash
cd /home/shawn/papdieo/aur/stable
makepkg --printsrcinfo > .SRCINFO
```
