# Hardwave Workspace

**Project:** HW-WORKSPACE-V1  
**Domain:** workspace.hardwavestudios.com  
**Stack:** Next.js 16 · TypeScript · Tailwind v4 · MySQL · Hetzner Object Storage

## What This Is
A paid file storage and collaboration platform for anyone with a Hardwave account. Users log in with their existing Hardwave account (JWT). Workspaces are the billing unit — Starter (€25/mo, 10 members, 500 GB) or Professional (€50/mo, 50 members, 2 TB).

## Repository Structure
This repo tracks the workspace app source (`apps/workspace/` in the Hardwave monorepo). The full monorepo lives on the server at `/opt/hardwave/studio/`.

## Architecture
- **Auth:** Reuses existing Hardwave JWT — `verifyAuth()` from `@hardwave/shared/auth`
- **Storage:** Hetzner Object Storage (S3-compatible, EU Nuremberg) — presigned URL uploads
- **DB:** MySQL `fl_organizer_db` on static-pages server (46.225.219.184) — `ws_*` tables
- **Billing:** Stripe workspace-scoped subscriptions (separate from VST Pro subscriptions)
- **Deploy:** Docker container on vst-web01/02/03 cluster, port 3008

## Style Guide
Always fetch the live Hardwave style guide before building any UI:
```bash
curl https://erp.hardwavestudios.com/api/erp/style-guide
```

## Key Environment Variables
See `.env.example` for all required variables.

## Server Deployment
```bash
# On analyser/vst-web server
cd /opt/vst-webviews
docker compose build workspace
docker compose up -d workspace
docker compose logs -f workspace
```

## TLS Certificate
After DNS is pointed to the server, expand the cert:
```bash
certbot --nginx -d workspace.hardwavestudios.com
```

## Database Tables
- `ws_workspaces` — workspace entities (billing unit)
- `ws_workspace_members` — user-workspace membership with roles
- `ws_folders` — hierarchical folder structure
- `ws_files` — file metadata (bytes in S3)
- `ws_share_links` — public sharing with password + expiry
- `ws_subscriptions` — workspace-level Stripe subscriptions

## ERP Project
Track progress at: erp.hardwavestudios.com → Projects → HW-WORKSPACE-V1 (ID: 11)

## Document Archive
API key: `540f288f51ee8029e2d9c085c4ea0b58880dfdde8c68b33b66847829cbd5235e`  
Docs: https://erp.hardwavestudios.com/api/erp/archive/docs
