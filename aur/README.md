# AUR packaging files

This repository now includes two AUR package variants:

- `papdieo-git` (rolling package from latest git HEAD)
  - Files: `aur/PKGBUILD`, `aur/.SRCINFO`
- `papdieo` (stable package from release tags)
  - Files: `aur/stable/PKGBUILD`, `aur/stable/.SRCINFO`

## Which one to publish

- Publish `papdieo-git` immediately.
- Publish stable `papdieo` after pushing a release tag matching `v<pkgver>` (for example `v0.1.0`).

## Feature notes

- Recent config update: per-monitor fit mode is supported via `monitor_fit_modes`.
- Example:

```toml
monitor_fit_modes = { DP-4 = "cover", HDMI-A-2 = "contain" }
fit_mode = "cover"
```

- Fallback order: monitor-specific entry first, then global `fit_mode`.

## Publish commands

### papdieo-git

```bash
git clone ssh://aur@aur.archlinux.org/papdieo-git.git
cd papdieo-git
cp /home/shawn/papdieo/aur/PKGBUILD .
cp /home/shawn/papdieo/aur/.SRCINFO .
git add PKGBUILD .SRCINFO
git commit -m "Initial release of papdieo-git"
git push
```

### papdieo (stable)

```bash
# first ensure upstream tag exists
cd /home/shawn/papdieo
git tag -a v0.1.0 -m "v0.1.0"
git push origin v0.1.0

# then publish to AUR
git clone ssh://aur@aur.archlinux.org/papdieo.git
cd papdieo
cp /home/shawn/papdieo/aur/stable/PKGBUILD .
cp /home/shawn/papdieo/aur/stable/.SRCINFO .
git add PKGBUILD .SRCINFO
git commit -m "Initial stable release of papdieo"
git push
```
