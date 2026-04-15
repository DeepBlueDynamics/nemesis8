# Release checklist

**One rule: bump `Cargo.toml` version BEFORE pushing the tag. The tag and the version must match.**

```
# 1. Bump version in Cargo.toml
#    version = "X.Y.Z"

# 2. Commit + push main
git add Cargo.toml Cargo.lock
git commit -m "vX.Y.Z: <summary>"
git push origin main

# 3. Tag + push (triggers CI release build)
git tag vX.Y.Z
git push origin vX.Y.Z
```

Never `git tag` before the version bump commit is pushed.
