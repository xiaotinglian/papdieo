# AUR publish guide for papdieo

This folder contains the AUR packaging files for `papdieo-git`.

## Release notes

- Recent config update: per-monitor fit mode support via `monitor_fit_modes`.
- Example:

```toml
monitor_fit_modes = { DP-4 = "cover", HDMI-A-2 = "contain" }
fit_mode = "cover"
```

- Fallback behavior: if a monitor is not in `monitor_fit_modes`, global `fit_mode` is used.

## 1) Test package locally

```bash
cd aur
makepkg -sfc
```

## 2) Generate `.SRCINFO`

```bash
cd aur
makepkg --printsrcinfo > .SRCINFO
```

## 3) Publish to AUR

Create package base on AUR first (web UI), then:

```bash
git clone ssh://aur@aur.archlinux.org/papdieo-git.git
cd papdieo-git
cp /home/shawn/papdieo/aur/PKGBUILD .
cp /home/shawn/papdieo/aur/.SRCINFO .
git add PKGBUILD .SRCINFO
git commit -m "Initial release of papdieo-git"
git push
```

## 4) Update after changes

```bash
# in your local packaging folder
cd /home/shawn/papdieo/aur
# bump pkgrel if packaging-only change; do not bump pkgrel for source-only git updates
makepkg --printsrcinfo > .SRCINFO

# in your AUR clone
cd /path/to/papdieo-git
git pull
cp /home/shawn/papdieo/aur/PKGBUILD .
cp /home/shawn/papdieo/aur/.SRCINFO .
git add PKGBUILD .SRCINFO
git commit -m "Update packaging"
git push
```
