# Copyright Materialize, Inc. and contributors. All rights reserved.
#
# Use of this software is governed by the Business Source License
# included in the LICENSE file at the root of this repository.
#
# As of the Change Date specified in that file, in accordance with
# the Business Source License, use of this software will be governed
# by the Apache License, Version 2.0.
from textwrap import dedent

from materialize.checks.actions import Testdrive
from materialize.checks.checks import Check
from materialize.checks.common import KAFKA_SCHEMA_WITH_SINGLE_STRING_FIELD


def schemas() -> str:
    return dedent(KAFKA_SCHEMA_WITH_SINGLE_STRING_FIELD)


class UpsertInsert(Check):
    """Test that repeated inserts of the same record are properly handled"""

    def initialize(self) -> Testdrive:
        return Testdrive(
            schemas()
            + dedent(
                """
                $ kafka-create-topic topic=upsert-insert

                $ kafka-ingest format=avro key-format=avro topic=upsert-insert key-schema=${keyschema} schema=${schema} repeat=10000
                {"key1": "A${kafka-ingest.iteration}"} {"f1": "A${kafka-ingest.iteration}"}

                > CREATE SOURCE upsert_insert_src
                  FROM KAFKA CONNECTION kafka_conn (TOPIC 'testdrive-upsert-insert-${testdrive.seed}')
                > CREATE TABLE upsert_insert FROM SOURCE upsert_insert_src (REFERENCE "testdrive-upsert-insert-${testdrive.seed}")
                  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY CONNECTION csr_conn
                  ENVELOPE UPSERT

                > CREATE MATERIALIZED VIEW upsert_insert_view AS SELECT COUNT(DISTINCT key1 || ' ' || f1) FROM upsert_insert;
                """
            )
        )

    def manipulate(self) -> list[Testdrive]:
        return [
            Testdrive(schemas() + dedent(s))
            for s in [
                """
                $ kafka-ingest format=avro key-format=avro topic=upsert-insert key-schema=${keyschema} schema=${schema} repeat=10000
                {"key1": "A${kafka-ingest.iteration}"} {"f1": "A${kafka-ingest.iteration}"}
                """,
                """
                $ kafka-ingest format=avro key-format=avro topic=upsert-insert key-schema=${keyschema} schema=${schema} repeat=10000
                {"key1": "A${kafka-ingest.iteration}"} {"f1": "A${kafka-ingest.iteration}"}
                """,
            ]
        ]

    def validate(self) -> Testdrive:
        return Testdrive(
            dedent(
                """
                > SELECT COUNT(*), COUNT(DISTINCT key1), COUNT(DISTINCT f1) FROM upsert_insert
                10000 10000 10000

                > SELECT * FROM upsert_insert_view;
                10000
           """
            )
        )


class UpsertUpdate(Check):
    def initialize(self) -> Testdrive:
        return Testdrive(
            schemas()
            + dedent(
                """
                $ kafka-create-topic topic=upsert-update

                $ kafka-ingest format=avro key-format=avro topic=upsert-update key-schema=${keyschema} schema=${schema} repeat=10000
                {"key1": "${kafka-ingest.iteration}"} {"f1": "A${kafka-ingest.iteration}"}

                > CREATE SOURCE upsert_update_src
                  FROM KAFKA CONNECTION kafka_conn (TOPIC 'testdrive-upsert-update-${testdrive.seed}')
                > CREATE TABLE upsert_update FROM SOURCE upsert_update_src (REFERENCE "testdrive-upsert-update-${testdrive.seed}")
                  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY CONNECTION csr_conn
                  ENVELOPE UPSERT

                > CREATE MATERIALIZED VIEW upsert_update_view AS SELECT LEFT(f1, 1), COUNT(*) AS c1, COUNT(DISTINCT key1) AS c2, COUNT(DISTINCT f1) AS c3 FROM upsert_update GROUP BY LEFT(f1, 1);
                """
            )
        )

    def manipulate(self) -> list[Testdrive]:
        return [
            Testdrive(schemas() + dedent(s))
            for s in [
                """
                $ kafka-ingest format=avro key-format=avro topic=upsert-update key-schema=${keyschema} schema=${schema} repeat=10000
                {"key1": "${kafka-ingest.iteration}"} {"f1": "B${kafka-ingest.iteration}"}
                """,
                """
                $ kafka-ingest format=avro key-format=avro topic=upsert-update key-schema=${keyschema} schema=${schema} repeat=10000
                {"key1": "${kafka-ingest.iteration}"} {"f1": "C${kafka-ingest.iteration}"}
                """,
            ]
        ]

    def validate(self) -> Testdrive:
        return Testdrive(
            dedent(
                """
                > SELECT * FROM upsert_update_view;
                C 10000 10000 10000
           """
            )
        )


class UpsertDelete(Check):
    def initialize(self) -> Testdrive:
        return Testdrive(
            schemas()
            + dedent(
                """
                $ kafka-create-topic topic=upsert-delete

                $ kafka-ingest format=avro key-format=avro topic=upsert-delete key-schema=${keyschema} schema=${schema} repeat=30000
                {"key1": "${kafka-ingest.iteration}"} {"f1": "${kafka-ingest.iteration}"}

                > CREATE SOURCE upsert_delete_src
                  FROM KAFKA CONNECTION kafka_conn (TOPIC 'testdrive-upsert-delete-${testdrive.seed}')
                > CREATE TABLE upsert_delete FROM SOURCE upsert_delete_src (REFERENCE "testdrive-upsert-delete-${testdrive.seed}")
                  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY CONNECTION csr_conn
                  ENVELOPE UPSERT

                > CREATE MATERIALIZED VIEW upsert_delete_view AS SELECT COUNT(*), MIN(key1), MAX(key1) FROM upsert_delete;
                """
            )
        )

    def manipulate(self) -> list[Testdrive]:
        return [
            Testdrive(schemas() + dedent(s))
            for s in [
                """
                $ kafka-ingest format=avro key-format=avro topic=upsert-delete key-schema=${keyschema} schema=${schema} repeat=10000
                {"key1": "${kafka-ingest.iteration}"}
                """,
                """
                $ kafka-ingest format=avro key-format=avro topic=upsert-delete key-schema=${keyschema} schema=${schema} start-iteration=20000 repeat=10000
                {"key1": "${kafka-ingest.iteration}"}
                """,
            ]
        ]

    def validate(self) -> Testdrive:
        return Testdrive(
            dedent(
                """
                > SELECT * FROM upsert_delete_view;
                10000 10000 19999
           """
            )
        )


class UpsertLegacy(Check):
    """
    An upsert source test that uses the legacy syntax to create the source
    on all versions to ensure the source is properly migrated with the
    ActivateSourceVersioningMigration scenario
    """

    def initialize(self) -> Testdrive:
        return Testdrive(
            schemas()
            + dedent(
                """
                $ kafka-create-topic topic=upsert-legacy-syntax

                $ kafka-ingest format=avro key-format=avro topic=upsert-legacy-syntax key-schema=${keyschema} schema=${schema} repeat=10000
                {"key1": "A${kafka-ingest.iteration}"} {"f1": "A${kafka-ingest.iteration}"}

                > CREATE SOURCE upsert_insert_legacy
                  FROM KAFKA CONNECTION kafka_conn (TOPIC 'testdrive-upsert-legacy-syntax-${testdrive.seed}')
                  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY CONNECTION csr_conn
                  ENVELOPE UPSERT

                > CREATE MATERIALIZED VIEW upsert_insert_legacy_view AS SELECT COUNT(DISTINCT key1 || ' ' || f1) FROM upsert_insert_legacy;
                """
            )
        )

    def manipulate(self) -> list[Testdrive]:
        return [
            Testdrive(schemas() + dedent(s))
            for s in [
                """
                $ kafka-ingest format=avro key-format=avro topic=upsert-legacy-syntax key-schema=${keyschema} schema=${schema} repeat=10000
                {"key1": "A${kafka-ingest.iteration}"} {"f1": "A${kafka-ingest.iteration}"}
                """,
                """
                $ kafka-ingest format=avro key-format=avro topic=upsert-legacy-syntax key-schema=${keyschema} schema=${schema} repeat=10000
                {"key1": "A${kafka-ingest.iteration}"} {"f1": "A${kafka-ingest.iteration}"}
                """,
            ]
        ]

    def validate(self) -> Testdrive:
        return Testdrive(
            dedent(
                """
                > SELECT COUNT(*), COUNT(DISTINCT key1), COUNT(DISTINCT f1) FROM upsert_insert_legacy
                10000 10000 10000

                > SELECT * FROM upsert_insert_legacy_view;
                10000
           """
            )
        )
