# syntax=docker/dockerfile:1

######################################################################################
# Base
######################################################################################
FROM node:20-alpine AS base
WORKDIR /app
ENV NODE_ENV=production

######################################################################################
# Dependencies (includes devDependencies, needed to compile TS and native modules)
######################################################################################
FROM base AS deps
# python3/make/g++ are required to build the sqlite3 native addon on alpine (musl)
RUN apk add --no-cache python3 make g++
COPY package.json package-lock.json ./
RUN npm ci

######################################################################################
# Build (compile TypeScript)
######################################################################################
FROM deps AS build
COPY tsconfig.json ./
COPY src ./src
RUN npm run build

######################################################################################
# Production dependencies only
######################################################################################
FROM base AS prod-deps
RUN apk add --no-cache python3 make g++
COPY package.json package-lock.json ./
RUN npm ci --omit=dev && npm cache clean --force

######################################################################################
# Runtime
######################################################################################
FROM node:20-alpine AS runtime
WORKDIR /app
ENV NODE_ENV=production

RUN addgroup -S cookie && adduser -S cookie -G cookie

COPY --from=prod-deps /app/node_modules ./node_modules
COPY --from=build /app/dist ./dist
COPY package.json ./

RUN mkdir -p /app/data && chown -R cookie:cookie /app

USER cookie

VOLUME ["/app/data"]

# Discord bots are outbound-gateway only; no port is exposed.
CMD ["node", "dist/index.js"]
