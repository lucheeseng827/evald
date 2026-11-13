# evald @VERSION@ air-gap bundle (@TARGET@)

Everything an install or upgrade of evald @VERSION@ needs on a machine with no network. Nothing
below makes a network call.

## 1. Verify it

Bring a Sigstore trusted root in separately from this bundle: on a connected machine run
`cosign initialize`, then copy `trusted_root.json` from `~/.sigstore/root/targets/` (cosign 2)
or `~/.sigstore/root/tuf-repo-cdn.sigstore.dev/targets/` (cosign 3).
Then, on the air-gapped machine, with cosign 2.4 or later:

```sh
sh verify.sh --trusted-root /path/to/trusted_root.json
```

`verify.sh` checks every file here against `SHA256SUMS`: nothing missing, nothing altered and
nothing added. It then checks that `SHA256SUMS` was signed by evald's release workflow, using the
keyless signature bundle (`SHA256SUMS.sigstore.json`). This verifies offline: the bundle carries
the transparency-log proof that a detached signature would need the network for.

If your organisation re-signs imports with its own key, sign `SHA256SUMS` on the import side and
check that signature instead:

```sh
cosign sign-blob --yes --key import.key --tlog-upload=false \
  --output-signature SHA256SUMS.sig SHA256SUMS          # cosign 2.x
cosign sign-blob --yes --key import.key --tlog-upload=false \
  --use-signing-config=false --new-bundle-format=false \
  --output-signature SHA256SUMS.sig SHA256SUMS          # cosign 3.x
sh verify.sh --key import.pub
```

Both make no network call.

## 2. Before an upgrade: the preflight

```sh
tar -xzf evald-@TARGET@.tar.gz
./evald migrate --check --data-dir /var/lib/evald
```

It reads the data-dir and writes nothing. It reports whether this evald reads the store's
on-disk format, whether a running evald holds it, and the free space beside it. It exits 1 when
this evald cannot take over the data-dir. A data-dir is never downgraded.

## 3. Install

**Binary** (systemd, a VM):

```sh
sudo systemctl stop evald
sudo install -m 0755 evald /usr/local/bin/evald
sudo systemctl start evald
```

**Container**: load the image and push it to your own registry:

```sh
docker load -i evald-image.tar        # or: podman load -i evald-image.tar
docker tag evald:@VERSION@ registry.internal/evald:@VERSION@
docker push registry.internal/evald:@VERSION@
```

**Kubernetes**: install the chart from this bundle, pointing it at that registry:

```sh
helm upgrade --install evald ./evald-@VERSION@.tgz \
  --set image.repository=registry.internal/evald --set image.tag=@VERSION@
```

Back up the data-dir first, and keep the previous binary or image to roll back to. The full
procedure is in evald's docs/OPERATIONS.md, under Upgrade and Air-gapped install.
