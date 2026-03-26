#!/bin/bash
set -e

PROJECT_ID="gnosis-459403"
REGION="us-central1"
SERVICE="pokeball-registry"
IMAGE="gcr.io/${PROJECT_ID}/${SERVICE}"

echo "==> Building + pushing"
gcloud builds submit --tag $IMAGE .

echo "==> Deploying to Cloud Run"
gcloud run deploy $SERVICE \
  --image $IMAGE \
  --region $REGION \
  --platform managed \
  --allow-unauthenticated \
  --port 8080 \
  --memory 256Mi \
  --cpu 1 \
  --min-instances 0 \
  --max-instances 3 \
  --set-env-vars "GITHUB_TOKEN=${GITHUB_TOKEN},REGISTRY_REPO=DeepBlueDynamics/pokeball-registry"

echo "==> Done"
gcloud run services describe $SERVICE --region $REGION --format 'value(status.url)'
