---
title: "SHOW TYPES"
description: "`SHOW TYPES` returns a list of the data types in Materialize."
menu:
  main:
    parent: commands
---

`SHOW TYPES` returns a list of the data types in Materialize. By default, only custom types are returned.

## Syntax

{{< diagram "show-types.svg" >}}

Field | Use
------|-----
_schema&lowbar;name_ | The schema to show types from. Defaults to first resolvable schema in the search path. For available schemas, see [`SHOW SCHEMAS`](../show-schemas).


## Examples

### Show custom data types

```mzsql
SHOW TYPES;
```
```
   name
-----------
 int4_list
```

## Related pages

* [`CREATE TYPE`](../create-type)
* [`DROP TYPE`](../drop-type)
