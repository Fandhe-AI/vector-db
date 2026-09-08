#!/bin/bash
cd /home/fandhe/fandhe/app/vector-db
until grep -q PULL_OK /home/fandhe/scratch455/crossdb-20260908/pull.log; do sleep 5; done
docker image inspect qdrant/qdrant:latest --format 'qdrant_latest={{index .RepoDigests 0}} {{.Created}}'
export CROSSDB_DIR=/home/fandhe/scratch455/crossdb-20260908 CROSSDB_PYTHON=/home/fandhe/scratch455/crossdb-20260908/venv/bin/python
export CROSSDB_PG_CONTAINER=bench-pgvector-0908 CROSSDB_PG_PORT=25433
export CROSSDB_QDRANT_CONTAINER=bench-qdrant-0908 CROSSDB_QDRANT_HTTP_PORT=26333 CROSSDB_QDRANT_GRPC_PORT=26334
export CROSSDB_MYSQL_CONTAINER=bench-mysql-0908 CROSSDB_MYSQL_PORT=43306
s=$(date +%s); make bench-crossdb; echo "CROSSDB_DONE rc=$? dur=$(( $(date +%s)-s ))"
