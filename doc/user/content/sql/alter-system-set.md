---
title: "ALTER SYSTEM SET"
description: "`ALTER SYSTEM SET` globally modifies the value of a configuration parameter."
menu:
  main:
    parent: 'commands'

---

`ALTER SYSTEM SET` globally modifies the value of a configuration parameter.

To see the current value of a configuration parameter, use [`SHOW`](../show).

## Syntax

{{< diagram "alter-system-set-stmt.svg" >}}

Field                   | Use
------------------------|-----
_name_                  | The name of the configuration parameter to modify.
_value_                 | The value to assign to the configuration parameter.
**DEFAULT**             | Reset the configuration parameter's default value. Equivalent to [`ALTER SYSTEM RESET`](../alter-system-reset).

{{% configuration-parameters %}}

## Privileges

The privileges required to execute this statement are:

{{< include-md
file="shared-content/sql-command-privileges/alter-system-set.md" >}}

## Related pages

- [`ALTER SYSTEM RESET`](../alter-system-reset)
- [`SHOW`](../show)
