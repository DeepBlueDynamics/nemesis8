# Release checklist

**One rule: bump `Cargo.toml` version BEFORE pushing the tag. The tag and the version must match.**

```
# 1. Bump version in Cargo.toml
#    version = "X.Y.Z"

# 2. Update Cargo.lock
cargo update --workspace

# 3. Commit + push main
git add Cargo.toml Cargo.lock
git commit -m "chore: bump to vX.Y.Z"
git push origin main

# 4. Tag + push (triggers CI release build)
git tag vX.Y.Z
git push origin vX.Y.Z
```

Never `git tag` before the version bump commit is pushed.

CI produces versioned artifacts: `nemisis8-vX.Y.Z-<target>.tar.gz` / `.zip`.
The binary version (`n8 -V`) must equal the tag — if they differ, the release is broken.
