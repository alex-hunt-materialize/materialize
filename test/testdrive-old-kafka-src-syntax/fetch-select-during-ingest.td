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
# Make sure that data that was ingested during the lifetime of a SELECT cursor is *NOT* FETCH-ed
#

$ postgres-execute connection=postgres://mz_system:materialize@${testdrive.materialize-internal-sql-addr}
ALTER SYSTEM SET min_timestamp_interval = '100ms'

$ set int={"type": "record", "name": "field_int", "fields": [ {"name": "f1", "type": "int"} ] }

$ kafka-create-topic topic=tail-fetch-during-ingest

> CREATE CONNECTION kafka_conn
  TO KAFKA (BROKER '${testdrive.kafka-addr}', SECURITY PROTOCOL PLAINTEXT);

> CREATE SOURCE fetch_during_ingest
  IN CLUSTER ${arg.single-replica-cluster}
  FROM KAFKA CONNECTION kafka_conn (TOPIC 'testdrive-tail-fetch-during-ingest-${testdrive.seed}')
  FORMAT AVRO USING SCHEMA '${int}'
  ENVELOPE NONE
  WITH (TIMESTAMP INTERVAL '100ms')

$ kafka-ingest format=avro topic=tail-fetch-during-ingest schema=${int} timestamp=1
{"f1": 123}

> SELECT * FROM fetch_during_ingest;
123

> BEGIN

> DECLARE c CURSOR FOR SELECT * FROM fetch_during_ingest;

> FETCH 1 c WITH (timeout='60s');
123

$ kafka-ingest format=avro topic=tail-fetch-during-ingest schema=${int} timestamp=2
{"f1": 234}

# Sleep here to make sure the entire machinery has run. Since we are in a transaction,
# we have no way of knowing that the source has progressed to '234' outside of the transaction

# NOTE(benesch): grumble. This is not a particularly robust way to write this
# test. It is, however, better than what was previously here, which used
# `SELECT mz_unsafe.mz_sleep(2)`, which had the extremely suboptimal property
# of wedging up the coordinator for 2s, instead of just pausing the test for 2s.
$ sleep-is-probably-flaky-i-have-justified-my-need-with-a-comment duration=2s

# This will return an empty result - nothing else available for fetching in the current transaction
> FETCH 1 c WITH (timeout='2s');

> COMMIT;

#
# The '234' row can now be fetched
#

> BEGIN

> DECLARE c CURSOR FOR SELECT * FROM fetch_during_ingest;

> FETCH 2 c WITH (timeout='60s');
123
234
