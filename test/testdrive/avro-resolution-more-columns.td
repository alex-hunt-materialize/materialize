# Copyright Materialize, Inc. and contributors. All rights reserved.
#
# Use of this software is governed by the Business Source License
# included in the LICENSE file at the root of this repository.
#
# As of the Change Date specified in that file, in accordance with
# the Business Source License, use of this software will be governed
# by the Apache License, Version 2.0.

$ set-arg-default single-replica-cluster=quickstart

#
# Test writing more columns in a source that has less columns. The extra columns are ignored
#

$ set 1column={"type": "record", "name": "schema_more_columns", "fields": [ {"name": "f1", "type": "string"} ] }
$ set 2columns={"type": "record", "name": "schema_more_columns", "fields": [ {"name": "f1", "type": "string"} , {"name": "f2", "type": "string", "default": "default_f2"}] }

$ kafka-create-topic topic=resolution-1to2

$ kafka-ingest format=avro topic=resolution-1to2 schema=${1column} timestamp=1
{"f1": "val_f1b"}

> CREATE CONNECTION IF NOT EXISTS csr_conn TO CONFLUENT SCHEMA REGISTRY (
    URL '${testdrive.schema-registry-url}'
  );

> CREATE CONNECTION kafka_conn
  TO KAFKA (BROKER '${testdrive.kafka-addr}', SECURITY PROTOCOL PLAINTEXT);

> CREATE SOURCE resolution_1to2
  IN CLUSTER ${arg.single-replica-cluster}
  FROM KAFKA CONNECTION kafka_conn (TOPIC 'testdrive-resolution-1to2-${testdrive.seed}')

> CREATE TABLE resolution_1to2_tbl FROM SOURCE resolution_1to2 (REFERENCE "testdrive-resolution-1to2-${testdrive.seed}")
  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY CONNECTION csr_conn
  ENVELOPE NONE

$ kafka-ingest format=avro topic=resolution-1to2 schema=${2columns} timestamp=2
{"f1": "val_f1a", "f2": "val_f2a"}

> SELECT * FROM resolution_1to2_tbl
f1
---
val_f1a
val_f1b
