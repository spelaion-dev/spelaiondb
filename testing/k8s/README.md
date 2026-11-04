# Kubernetes test cluster

A PostgreSQL inside a local Kubernetes cluster, for the Kubernetes connection tests (ADR-015).
It uses [kind](https://kind.sigs.k8s.io/) (Kubernetes in Docker) with its own kubeconfig in
this folder, so your `~/.kube/config` is not touched. `bin/` and `kubeconfig` are ignored by git.

```bash
kind create cluster --name spelaion-test --kubeconfig testing/k8s/kubeconfig --wait 180s
```

```bash
kubectl --kubeconfig testing/k8s/kubeconfig apply -f testing/k8s/postgres.yaml
```

```bash
kubectl --kubeconfig testing/k8s/kubeconfig -n spelaion-test rollout status deployment/postgres
```

Then run the tests with the variable pointing at the kubeconfig (an absolute path):

```bash
SPELAION_TEST_K8S="$PWD/testing/k8s/kubeconfig" cargo test -p spelaion-core --test kube
```

The smoke test (`apps/desktop/e2e/smoke.mjs`) also runs its Kubernetes step when the variable
is set. To remove the cluster:

```bash
kind delete cluster --name spelaion-test --kubeconfig testing/k8s/kubeconfig
```
