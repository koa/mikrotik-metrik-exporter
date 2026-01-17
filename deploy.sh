#!/bin/bash

dockerImage=ghcr.io/koa/mikrotik-metrik-exporter
kubernetesNamespace=mikrotik-exporter
kubernetesArtifact=mikrotik-exporter
kubernetesCluster=berg-main
kubernetesContainer=mikrotik-exporter
kubernetesObject=deployment
kubernetesContext="$(kubectl config get-contexts --no-headers=true | grep "$kubernetesCluster" | sed 's/  */ /g' | cut -d ' ' -f 2 | head -1)"

version=$(date +%Y-%m-%d-%H-%M-%S)-$USER
targetImage=$dockerImage:$version

podman build . -t $targetImage || exit 1
podman push $targetImage || exit 1
kubectl --context="$kubernetesContext" -n "$kubernetesNamespace" set image $kubernetesObject/$kubernetesArtifact "$kubernetesContainer"="$targetImage"
kubectl --context="$kubernetesContext" -n $kubernetesNamespace annotate $kubernetesObject/$kubernetesArtifact kubernetes.io/change-cause="deploy image $targetImage" --record=false --overwrite=true
kubectl --context="$kubernetesContext" -n $kubernetesNamespace rollout history $kubernetesObject/$kubernetesArtifact
kubectl --context="$kubernetesContext" -n $kubernetesNamespace rollout status -w $kubernetesObject/$kubernetesArtifact

