//! Some utilities for working with arrow data types

use std::collections::HashSet;
use std::io::{BufRead, BufReader};
use std::sync::Arc;

use crate::engine::arrow_conversion::{TryFromKernel as _, TryIntoArrow as _};
use crate::engine::ensure_data_types::DataTypeCompat;
use crate::{
    engine::arrow_data::ArrowEngineData,
    schema::{DataType, Schema, SchemaRef, StructField, StructType},
    utils::require,
    DeltaResult, EngineData, Error,
};

use crate::arrow::array::{
    cast::AsArray, make_array, new_null_array, Array as ArrowArray, GenericListArray, MapArray,
    OffsetSizeTrait, RecordBatch, StringArray, StructArray,
};
use crate::arrow::buffer::NullBuffer;
use crate::arrow::compute::concat_batches;
use crate::arrow::datatypes::{
    DataType as ArrowDataType, Field as ArrowField, FieldRef as ArrowFieldRef, Fields,
    Schema as ArrowSchema, SchemaRef as ArrowSchemaRef,
};
use crate::arrow::json::{LineDelimitedWriter, ReaderBuilder};
use crate::parquet::{arrow::ProjectionMask, schema::types::SchemaDescriptor};
use delta_kernel_derive::internal_api;
use itertools::Itertools;
use tracing::debug;

macro_rules! prim_array_cmp {
    ( $left_arr: ident, $right_arr: ident, $(($data_ty: pat, $prim_ty: ty)),+ ) => {

        return match $left_arr.data_type() {
        $(
            $data_ty => {
                let prim_array = $left_arr.as_primitive_opt::<$prim_ty>()
                        .ok_or(Error::invalid_expression(
                            format!("Cannot cast to primitive array: {}", $left_arr.data_type()))
                        )?;
                    let list_array = $right_arr.as_list_opt::<i32>()
                        .ok_or(Error::invalid_expression(
                            format!("Cannot cast to list array: {}", $right_arr.data_type()))
                        )?;
                crate::arrow::compute::kernels::comparison::in_list(prim_array, list_array)
            }
        )+
            _ => Err(ArrowError::CastError(
                        format!("Bad Comparison between: {:?} and {:?}",
                            $left_arr.data_type(),
                            $right_arr.data_type())
                        )
                )
        }.map_err(Error::generic_err);
    };
}

pub(crate) use prim_array_cmp;

/// Get the indices in `parquet_schema` of the specified columns in `requested_schema`. This
/// returns a tuples of (mask_indices: Vec<parquet_schema_index>, reorder_indices:
/// Vec<requested_index>). `mask_indices` is used for generating the mask for reading from the
pub(crate) fn make_arrow_error(s: impl Into<String>) -> Error {
    Error::Arrow(crate::arrow::error::ArrowError::InvalidArgumentError(
        s.into(),
    ))
    .with_backtrace()
}

/// Applies post-processing to data read from parquet files. This includes `reorder_struct_array` to
/// ensure schema compatibility, as well as `fix_nested_null_masks` to ensure that leaf columns have
/// accurate null masks that row visitors rely on for correctness.
pub(crate) fn fixup_parquet_read<T>(
    batch: RecordBatch,
    requested_ordering: &[ReorderIndex],
) -> DeltaResult<T>
where
    StructArray: Into<T>,
{
    let data = reorder_struct_array(batch.into(), requested_ordering)?;
    let data = fix_nested_null_masks(data);
    Ok(data.into())
}

/*
* The code below implements proper pruning of columns when reading parquet, reordering of columns to
* match the specified schema, and insertion of null columns if the requested schema includes a
* nullable column that isn't included in the parquet file.
*
* At a high level there are three schemas/concepts to worry about:
*  - The parquet file's physical schema (= the columns that are actually available), called
*    "parquet_schema" below
*  - The requested logical schema from the engine (= the columns we actually want), called
*    "requested_schema" below
*  - The Read schema (and intersection of 1. and 2., in logical schema order). This is never
*    materialized, but is useful to be able to refer to here
*  - A `ProjectionMask` that goes to the parquet reader which specifies which subset of columns from
*    the file schema to actually read. (See "Example" below)
*
* In other words, the ProjectionMask is the intersection of the parquet schema and logical schema,
* and then mapped to indices in the parquet file. Columns unique to the file schema need to be
* masked out (= ignored), while columns unique to the logical schema need to be backfilled with
* nulls.
*
* We also have to worry about field ordering differences between the read schema and logical
* schema. We represent any reordering needed as a tree. Each level of the tree is a vec of
* `ReorderIndex`s. Each element's index represents a column that will be in the read parquet data
* (as an arrow StructArray) at that level and index. The `ReorderIndex::index` field of the element
* is the position that the column should appear in the final output.

* The algorithm has three parts, handled by `get_requested_indices`, `generate_mask` and
* `reorder_struct_array` respectively.

* `get_requested_indices` generates indices to select, along with reordering information:
* 1. Loop over each field in parquet_schema, keeping track of how many physical fields (i.e. leaf
*    columns) we have seen so far
* 2. If a requested field matches the physical field, push the index of the field onto the mask.

* 3. Also push a ReorderIndex element that indicates where this item should be in the final output,
*    and if it needs any transformation (i.e. casting, create null column)
* 4. If a nested element (struct/map/list) is encountered, recurse into it, pushing indices onto
*    the same vector, but producing a new reorder level, which is added to the parent with a `Nested`
*    transform
*
* `generate_mask` is simple, and just calls `ProjectionMask::leaves` in the parquet crate with the
* indices computed by `get_requested_indices`
*
* `reorder_struct_array` handles reordering and data transforms:
* 1. First check if we need to do any transformations (see doc comment for
*    `ordering_needs_transform`)
* 2. If nothing is required we're done (return); otherwise:
* 3. Create a Vec[None, ..., None] of placeholders that will hold the correctly ordered columns
* 4. Deconstruct the existing struct array and then loop over the `ReorderIndex` list
* 5. Use the `ReorderIndex::index` value to put the column at the correct location
* 6. Additionally, if `ReorderIndex::transform` is not `Identity`, then if it is:
*      - `Cast`: cast the column to the specified type
*      - `Missing`: put a column of `null` at the correct location
*      - `Nested([child_order])` and the data is a `StructArray`: recursively call
*         `reorder_struct_array` on the column with `child_order` to correctly ordered the child
*         array
*      - `Nested` and the data is a `List<StructArray>`: get the inner struct array out of the list,
*         reorder it recursively as above, rebuild the list, and the put the column at the correct
*         location
*      - `Nested` and the data is a `Map`. We expect the child order to contain two elements. The
*         first specifies any needed reordering in the keys (i.e. if the key contains a struct),
*         and the second any reordering needed in the values.
*
* Example:
* The parquet crate `ProjectionMask::leaves` method only considers leaf columns -- a "flat" schema --
* so a struct column is purely a schema level thing and doesn't "count" wrt. column indices.
*
* So if we have the following file physical schema:
*
*  a
*    d
*    x
*  b
*    y
*      z
*    e
*    f
*  c
*
* and a logical requested schema of:
*
*  b
*    f
*    e
*  a
*    x
*  c
*
* The mask is [1, 3, 4, 5] because a, b, and y don't contribute to the column indices.
*
* The reorder tree is:
* [
*   // col a is at position 0 in the struct array, and should be moved to position 1
*   { index: 1, Nested([{ index: 0 }]) },
*   // col b is at position 1 in the struct array, and should be moved to position 0
*   //   also, the inner struct array needs to be reordered to swap 'f' and 'e'
*   { index: 0, Nested([{ index: 1 }, {index: 0}]) },
*   // col c is at position 2 in the struct array, and should stay there
*   { index: 2 }
* ]
*/

/// Reordering is specified as a tree. Each level is a vec of `ReorderIndex`s. Each element's
/// position represents a column that will be in the read parquet data at that level and
/// position. The `index` of the element is the position that the column should appear in the final
/// output. The `transform` indicates what, if any, transforms are needed. See the docs for
/// [`ReorderIndexTransform`] for the meaning.
#[derive(Debug, PartialEq)]
pub(crate) struct ReorderIndex {
    pub(crate) index: usize,
    transform: ReorderIndexTransform,
}

#[derive(Debug, PartialEq)]
pub(crate) enum ReorderIndexTransform {
    /// For a non-nested type, indicates that we need to cast to the contained type
    Cast(ArrowDataType),
    /// Used for struct/list/map. Potentially transform child fields using contained reordering
    Nested(Vec<ReorderIndex>),
    /// No work needed to transform this data
    Identity,
    /// Data is missing, fill in with a null column
    Missing(ArrowFieldRef),
}

impl ReorderIndex {
    fn new(index: usize, transform: ReorderIndexTransform) -> Self {
        ReorderIndex { index, transform }
    }

    fn cast(index: usize, target: ArrowDataType) -> Self {
        ReorderIndex::new(index, ReorderIndexTransform::Cast(target))
    }

    fn nested(index: usize, children: Vec<ReorderIndex>) -> Self {
        ReorderIndex::new(index, ReorderIndexTransform::Nested(children))
    }

    fn identity(index: usize) -> Self {
        ReorderIndex::new(index, ReorderIndexTransform::Identity)
    }

    fn missing(index: usize, field: ArrowFieldRef) -> Self {
        ReorderIndex::new(index, ReorderIndexTransform::Missing(field))
    }

    /// Check if this reordering requires a transformation anywhere. See comment below on
    /// [`ordering_needs_transform`] to understand why this is needed.
    fn needs_transform(&self) -> bool {
        match self.transform {
            // if we're casting or inserting null, we need to transform
            ReorderIndexTransform::Cast(_) | ReorderIndexTransform::Missing(_) => true,
            // if our nested ordering needs a transform, we need a transform
            ReorderIndexTransform::Nested(ref children) => ordering_needs_transform(children),
            // no transform needed
            ReorderIndexTransform::Identity => false,
        }
    }
}

// count the number of physical columns, including nested ones in an `ArrowField`
fn count_cols(field: &ArrowField) -> usize {
    _count_cols(field.data_type())
}

fn _count_cols(dt: &ArrowDataType) -> usize {
    match dt {
        ArrowDataType::Struct(fields) => fields.iter().map(|f| count_cols(f)).sum(),
        ArrowDataType::Union(fields, _) => fields.iter().map(|(_, f)| count_cols(f)).sum(),
        ArrowDataType::List(field)
        | ArrowDataType::LargeList(field)
        | ArrowDataType::FixedSizeList(field, _)
        | ArrowDataType::Map(field, _) => count_cols(field),
        ArrowDataType::Dictionary(_, value_field) => _count_cols(value_field.as_ref()),
        _ => 1, // other types are "real" fields, so count
    }
}

/// Validate that a given field in a parquet file which is presumed to represent data of the
/// `VARIANT` type is represented as `STRUCT<metadata: BINARY, value: BINARY>`. This is to make
/// sure that the default engine does not try to read shredded Variants, which it currently does
/// not support.
fn validate_parquet_variant(field: &ArrowField) -> DeltaResult<()> {
    fn variant_parquet_error(field_name: &String) -> Error {
        Error::Generic(format!(
            "The field {field_name} presumed to be of Variant type might be \
            shredded in the parquet file. The default engine does not support \
            shredded reads yet."
        ))
    }
    match field.data_type() {
        ArrowDataType::Struct(fields) => {
            if fields.len() != 2 {
                return Err(variant_parquet_error(field.name()));
            }
            if !matches!(
                (fields[0].name().as_str(), fields[1].name().as_str()),
                ("value", "metadata") | ("metadata", "value")
            ) {
                return Err(variant_parquet_error(field.name()));
            }
            Ok(())
        }
        _ => Err(variant_parquet_error(field.name())),
    }
}

/// helper function, does the same as `get_requested_indices` but at an offset. used to recurse into
/// structs, lists, and maps. `parquet_offset` is how many parquet fields exist before processing
/// this potentially nested schema. returns the number of parquet fields in `fields` (regardless of
/// if they are selected or not) and reordering information for the requested fields.
fn get_indices(
    start_parquet_offset: usize,
    requested_schema: &Schema,
    fields: &Fields,
    mask_indices: &mut Vec<usize>,
) -> DeltaResult<(usize, Vec<ReorderIndex>)> {
    let mut found_fields = HashSet::with_capacity(requested_schema.fields.len());
    let mut reorder_indices = Vec::with_capacity(requested_schema.fields.len());
    let mut parquet_offset = start_parquet_offset;
    // for each field, get its position in the parquet (via enumerate), a reference to the arrow
    // field, and info about where it appears in the requested_schema, or None if the field is not
    // requested
    let all_field_info = fields.iter().enumerate().map(|(parquet_index, field)| {
        let field_info = requested_schema.fields.get_full(field.name());
        (parquet_index, field, field_info)
    });
    for (parquet_index, field, field_info) in all_field_info {
        debug!(
            "Getting indices for field {} with offset {parquet_offset}, with index {parquet_index}",
            field.name()
        );
        if let Some((index, _, requested_field)) = field_info {
            // If the field is a variant, make sure the parquet schema matches the unshredded variant
            // representation. This is to ensure that shredded reads are not performed.
            if requested_field.data_type == DataType::unshredded_variant() {
                validate_parquet_variant(field)?;
            }
            match field.data_type() {
                ArrowDataType::Struct(fields) => {
                    if let DataType::Struct(ref requested_schema)
                    | DataType::Variant(ref requested_schema) = requested_field.data_type
                    {
                        let (parquet_advance, children) = get_indices(
                            parquet_index + parquet_offset,
                            requested_schema.as_ref(),
                            fields,
                            mask_indices,
                        )?;
                        // advance the number of parquet fields, but subtract 1 because the
                        // struct will be counted by the `enumerate` call but doesn't count as
                        // an actual index.
                        parquet_offset += parquet_advance - 1;
                        // note that we found this field
                        found_fields.insert(requested_field.name());
                        // push the child reorder on
                        reorder_indices.push(ReorderIndex::nested(index, children));
                    } else {
                        return Err(Error::unexpected_column_type(field.name()));
                    }
                }
                ArrowDataType::List(list_field)
                | ArrowDataType::LargeList(list_field)
                | ArrowDataType::ListView(list_field) => {
                    // we just want to transparently recurse into lists, need to transform the kernel
                    // list data type into a schema
                    if let DataType::Array(array_type) = requested_field.data_type() {
                        let requested_schema = StructType::new([StructField::new(
                            list_field.name().clone(), // so we find it in the inner call
                            array_type.element_type.clone(),
                            array_type.contains_null,
                        )]);
                        let (parquet_advance, mut children) = get_indices(
                            parquet_index + parquet_offset,
                            &requested_schema,
                            &[list_field.clone()].into(),
                            mask_indices,
                        )?;
                        // see comment above in struct match arm
                        parquet_offset += parquet_advance - 1;
                        found_fields.insert(requested_field.name());
                        if children.len() != 1 {
                            return Err(Error::generic(
                                "List call should not have generated more than one reorder index",
                            ));
                        }
                        // safety, checked that we have 1 element
                        let mut children = children.swap_remove(0);
                        // the index is wrong, as it's the index from the inner schema. Adjust
                        // it to be our index
                        children.index = index;
                        reorder_indices.push(children);
                    } else {
                        return Err(Error::unexpected_column_type(list_field.name()));
                    }
                }
                ArrowDataType::Map(key_val_field, _) => {
                    match (key_val_field.data_type(), requested_field.data_type()) {
                        (ArrowDataType::Struct(inner_fields), DataType::Map(map_type)) => {
                            let mut key_val_names =
                                inner_fields.iter().map(|f| f.name().to_string());
                            let key_name = key_val_names.next().ok_or_else(|| {
                                Error::generic("map fields didn't include a key col")
                            })?;
                            let val_name = key_val_names.next().ok_or_else(|| {
                                Error::generic("map fields didn't include a val col")
                            })?;
                            if key_val_names.next().is_some() {
                                return Err(Error::generic("map fields had more than 2 members"));
                            }
                            let inner_schema = map_type.as_struct_schema(key_name, val_name);
                            let (parquet_advance, mut children) = get_indices(
                                parquet_index + parquet_offset,
                                &inner_schema,
                                inner_fields,
                                mask_indices,
                            )?;

                            // advance the number of parquet fields, but subtract 1 because the
                            // map will be counted by the `enumerate` call but doesn't count as
                            // an actual index.
                            parquet_offset += parquet_advance - 1;
                            // note that we found this field
                            found_fields.insert(requested_field.name());

                            if children.len() != 2 {
                                return Err(Error::generic(
                                    "Map call should have generated exactly two reorder indices",
                                ));
                            }
                            // vec indexing is safe, we checked len above
                            let mut num_identity_transforms = 0;
                            if !children[0].needs_transform() {
                                children[0] = ReorderIndex::identity(0);
                                num_identity_transforms += 1;
                            }
                            if !children[1].needs_transform() {
                                children[1] = ReorderIndex::identity(1);
                                num_identity_transforms += 1;
                            }
                            let transform = match num_identity_transforms {
                                2 => ReorderIndex::identity(index),
                                _ => ReorderIndex::nested(index, children),
                            };
                            reorder_indices.push(transform);
                        }
                        _ => {
                            return Err(Error::unexpected_column_type(field.name()));
                        }
                    }
                }
                _ => {
                    // we don't care about matching on nullability or metadata here so pass `false`
                    // as the final argument. These can differ between the delta schema and the
                    // parquet schema without causing issues in reading the data. We fix them up in
                    // expression evaluation later.
                    match super::ensure_data_types::ensure_data_types(
                        &requested_field.data_type,
                        field.data_type(),
                        false,
                    )? {
                        DataTypeCompat::Identical => {
                            reorder_indices.push(ReorderIndex::identity(index))
                        }
                        DataTypeCompat::NeedsCast(target) => {
                            reorder_indices.push(ReorderIndex::cast(index, target))
                        }
                        DataTypeCompat::Nested => {
                            return Err(Error::internal_error(
                                "Comparing nested types in get_indices",
                            ))
                        }
                    }
                    found_fields.insert(requested_field.name());
                    mask_indices.push(parquet_offset + parquet_index);
                }
            }
        } else {
            // We're NOT selecting this field, but we still need to track how many leaf columns we
            // skipped over
            debug!("Skipping over un-selected field: {}", field.name());
            // offset by number of inner fields. subtract one, because the enumerate still
            // counts this logical "parent" field
            parquet_offset += count_cols(field) - 1;
        }
    }

    if found_fields.len() != requested_schema.fields.len() {
        // some fields are missing, but they might be nullable, need to insert them into the reorder_indices
        for (requested_position, field) in requested_schema.fields().enumerate() {
            if !found_fields.contains(field.name()) {
                if field.nullable {
                    debug!("Inserting missing and nullable field: {}", field.name());
                    reorder_indices.push(ReorderIndex::missing(
                        requested_position,
                        Arc::new(field.try_into_arrow()?),
                    ));
                } else {
                    return Err(Error::Generic(format!(
                        "Requested field not found in parquet schema, and field is not nullable: {}",
                        field.name()
                    )));
                }
            }
        }
    }
    Ok((
        parquet_offset + fields.len() - start_parquet_offset,
        reorder_indices,
    ))
}

/// Get the indices in `parquet_schema` of the specified columns in `requested_schema`. This returns
/// a tuple of (mask_indices: Vec<parquet_schema_index>, reorder_indices:
/// Vec<requested_index>). `mask_indices` is used for generating the mask for reading from the
/// parquet file, and simply contains an entry for each index we wish to select from the parquet
/// file set to the index of the requested column in the parquet. `reorder_indices` is used for
/// re-ordering. See the documentation for [`ReorderIndex`] to understand what each element in the
/// returned array means.
pub(crate) fn get_requested_indices(
    requested_schema: &SchemaRef,
    parquet_schema: &ArrowSchemaRef,
) -> DeltaResult<(Vec<usize>, Vec<ReorderIndex>)> {
    let mut mask_indices = vec![];
    let (_, reorder_indexes) = get_indices(
        0,
        requested_schema,
        parquet_schema.fields(),
        &mut mask_indices,
    )?;
    Ok((mask_indices, reorder_indexes))
}

/// Create a mask that will only select the specified indices from the parquet. `indices` can be
/// computed from a [`Schema`] using [`get_requested_indices`]
pub(crate) fn generate_mask(
    _requested_schema: &SchemaRef,
    _parquet_schema: &ArrowSchemaRef,
    parquet_physical_schema: &SchemaDescriptor,
    indices: &[usize],
) -> Option<ProjectionMask> {
    // TODO: Determine if it's worth checking if we're selecting everything and returning None in
    // that case
    Some(ProjectionMask::leaves(
        parquet_physical_schema,
        indices.to_owned(),
    ))
}

/// Check if an ordering requires transforming the data in any way.  This is true if the indices are
/// NOT in ascending order (so we have to reorder things), or if we need to do any transformation on
/// the data read from parquet. We check the ordering here, and also call
/// `ReorderIndex::needs_transform` on each element to check for other transforms, and to check
/// `Nested` variants recursively.
fn ordering_needs_transform(requested_ordering: &[ReorderIndex]) -> bool {
    if requested_ordering.is_empty() {
        return false;
    }
    // we have >=1 element. check that the first element doesn't need a transform
    if requested_ordering[0].needs_transform() {
        return true;
    }
    // Check for all elements if we need a transform. This is true if any elements are not in order
    // (i.e. element[i].index < element[i+1].index), or any element needs a transform
    requested_ordering
        .windows(2)
        .any(|ri| (ri[0].index >= ri[1].index) || ri[1].needs_transform())
}

// we use this as a placeholder for an array and its associated field. We can fill in a Vec of None
// of this type and then set elements of the Vec to Some(FieldArrayOpt) for each column
type FieldArrayOpt = Option<(Arc<ArrowField>, Arc<dyn ArrowArray>)>;

/// Reorder a RecordBatch to match `requested_ordering`. For each non-zero value in
/// `requested_ordering`, the column at that index will be added in order to returned batch
pub(crate) fn reorder_struct_array(
    input_data: StructArray,
    requested_ordering: &[ReorderIndex],
) -> DeltaResult<StructArray> {
    debug!("Reordering {input_data:?} with ordering: {requested_ordering:?}");
    if !ordering_needs_transform(requested_ordering) {
        // indices is already sorted, meaning we requested in the order that the columns were
        // stored in the parquet
        Ok(input_data)
    } else {
        // requested an order different from the parquet, reorder
        debug!("Have requested reorder {requested_ordering:#?} on {input_data:?}");
        let num_rows = input_data.len();
        let num_cols = requested_ordering.len();
        let (input_fields, input_cols, null_buffer) = input_data.into_parts();
        let mut final_fields_cols: Vec<FieldArrayOpt> = vec![None; num_cols];
        for (parquet_position, reorder_index) in requested_ordering.iter().enumerate() {
            // for each item, reorder_index.index() tells us where to put it, and its position in
            // requested_ordering tells us where it is in the parquet data
            match &reorder_index.transform {
                ReorderIndexTransform::Cast(target) => {
                    let col = input_cols[parquet_position].as_ref();
                    let col = Arc::new(crate::arrow::compute::cast(col, target)?);
                    let new_field = Arc::new(
                        input_fields[parquet_position]
                            .as_ref()
                            .clone()
                            .with_data_type(col.data_type().clone()),
                    );
                    final_fields_cols[reorder_index.index] = Some((new_field, col));
                }
                ReorderIndexTransform::Nested(children) => {
                    let input_field_name = input_fields[parquet_position].name();
                    match input_cols[parquet_position].data_type() {
                        ArrowDataType::Struct(_) => {
                            let struct_array = input_cols[parquet_position].as_struct().clone();
                            let result_array =
                                Arc::new(reorder_struct_array(struct_array, children)?);
                            // create the new field specifying the correct order for the struct
                            let new_field = Arc::new(ArrowField::new_struct(
                                input_field_name,
                                result_array.fields().clone(),
                                input_fields[parquet_position].is_nullable(),
                            ));
                            final_fields_cols[reorder_index.index] =
                                Some((new_field, result_array));
                        }
                        ArrowDataType::List(_) => {
                            let list_array = input_cols[parquet_position].as_list::<i32>().clone();
                            final_fields_cols[reorder_index.index] =
                                reorder_list(list_array, input_field_name, children)?;
                        }
                        ArrowDataType::LargeList(_) => {
                            let list_array = input_cols[parquet_position].as_list::<i64>().clone();
                            final_fields_cols[reorder_index.index] =
                                reorder_list(list_array, input_field_name, children)?;
                        }
                        ArrowDataType::Map(_, _) => {
                            let map_array = input_cols[parquet_position].as_map().clone();
                            final_fields_cols[reorder_index.index] =
                                reorder_map(map_array, input_field_name, children)?;
                        }
                        _ => {
                            return Err(Error::internal_error(
                                "Nested reorder can only apply to struct/list/map.",
                            ));
                        }
                    }
                }
                ReorderIndexTransform::Identity => {
                    final_fields_cols[reorder_index.index] = Some((
                        input_fields[parquet_position].clone(), // cheap Arc clone
                        input_cols[parquet_position].clone(),   // cheap Arc clone
                    ));
                }
                ReorderIndexTransform::Missing(field) => {
                    let null_array = Arc::new(new_null_array(field.data_type(), num_rows));
                    let field = field.clone(); // cheap Arc clone
                    final_fields_cols[reorder_index.index] = Some((field, null_array));
                }
            }
        }
        let num_cols = final_fields_cols.len();
        let (field_vec, reordered_columns): (Vec<Arc<ArrowField>>, _) =
            final_fields_cols.into_iter().flatten().unzip();
        if field_vec.len() != num_cols {
            Err(Error::internal_error("Found a None in final_fields_cols."))
        } else {
            Ok(StructArray::try_new(
                field_vec.into(),
                reordered_columns,
                null_buffer,
            )?)
        }
    }
}

fn reorder_list<O: OffsetSizeTrait>(
    list_array: GenericListArray<O>,
    input_field_name: &str,
    children: &[ReorderIndex],
) -> DeltaResult<FieldArrayOpt> {
    let (list_field, offset_buffer, maybe_sa, null_buf) = list_array.into_parts();
    if let Some(struct_array) = maybe_sa.as_struct_opt() {
        let struct_array = struct_array.clone();
        let result_array = Arc::new(reorder_struct_array(struct_array, children)?);
        let new_list_field = Arc::new(ArrowField::new_struct(
            list_field.name(),
            result_array.fields().clone(),
            result_array.is_nullable(),
        ));
        let new_field = Arc::new(ArrowField::new_list(
            input_field_name,
            new_list_field.clone(),
            list_field.is_nullable(),
        ));
        let list = Arc::new(GenericListArray::try_new(
            new_list_field,
            offset_buffer,
            result_array,
            null_buf,
        )?);
        Ok(Some((new_field, list)))
    } else {
        Err(Error::internal_error(
            "Nested reorder of list should have had struct child.",
        ))
    }
}

fn reorder_map(
    map_array: MapArray,
    input_field_name: &str,
    children: &[ReorderIndex],
) -> DeltaResult<FieldArrayOpt> {
    let (map_field, offset_buffer, struct_array, null_buf, ordered) = map_array.into_parts();
    let result_array = reorder_struct_array(struct_array, children)?;
    let result_fields = result_array.fields();
    let new_map_field = Arc::new(ArrowField::new_struct(
        map_field.name(),
        result_fields.clone(),
        result_array.is_nullable(),
    ));
    let key_field = result_fields[0].clone();
    let val_field = result_fields[1].clone();
    let new_field = Arc::new(ArrowField::new_map(
        input_field_name,
        map_field.name(),
        key_field,
        val_field,
        ordered,
        map_field.is_nullable(),
    ));
    let map = Arc::new(MapArray::try_new(
        new_map_field,
        offset_buffer,
        result_array,
        null_buf,
        ordered,
    )?);
    Ok(Some((new_field, map)))
}

/// Use this function to recursively compute properly unioned null masks for all nested
/// columns of a record batch, making it safe to project out and consume nested columns.
///
/// Arrow does not guarantee that the null masks associated with nested columns are accurate --
/// instead, the reader must consult the union of logical null masks the column and all
/// ancestors. The parquet reader stopped doing this automatically as of arrow-53.3, for example.
pub fn fix_nested_null_masks(batch: StructArray) -> StructArray {
    compute_nested_null_masks(batch, None)
}

/// Splits a StructArray into its parts, unions in the parent null mask, and uses the result to
/// recursively update the children as well before putting everything back together.
fn compute_nested_null_masks(sa: StructArray, parent_nulls: Option<&NullBuffer>) -> StructArray {
    let (fields, columns, nulls) = sa.into_parts();
    let nulls = NullBuffer::union(parent_nulls, nulls.as_ref());
    let columns = columns
        .into_iter()
        .map(|column| match column.as_struct_opt() {
            Some(sa) => Arc::new(compute_nested_null_masks(sa.clone(), nulls.as_ref())) as _,
            None => {
                let data = column.to_data();
                let nulls = NullBuffer::union(nulls.as_ref(), data.nulls());
                let builder = data.into_builder().nulls(nulls);
                // Use an unchecked build to avoid paying a redundant O(k) validation cost for a
                // `RecordBatch` with k leaf columns.
                //
                // SAFETY: The builder was constructed from an `ArrayData` we extracted from the
                // column. The change we make is the null buffer, via `NullBuffer::union` with input
                // null buffers that were _also_ extracted from the column and its parent. A union
                // can only _grow_ the set of NULL rows, so data validity is preserved. Even if the
                // `parent_nulls` somehow had a length mismatch --- which it never should, having
                // also been extracted from our grandparent --- the mismatch would have already
                // caused `NullBuffer::union` to panic.
                let data = unsafe { builder.build_unchecked() };
                make_array(data)
            }
        })
        .collect();

    // Use an unchecked constructor to avoid paying O(n*k) a redundant null buffer validation cost
    // for a `RecordBatch` with n rows and k leaf columns.
    //
    // SAFETY: We are simply reassembling the input `StructArray` we previously broke apart, with
    // updated null buffers. See above for details about null buffer safety.
    unsafe { StructArray::new_unchecked(fields, columns, nulls) }
}

/// Arrow lacks the functionality to json-parse a string column into a struct column -- even tho the
/// JSON file reader does exactly the same thing. This function is a hack to work around that gap.
#[internal_api]
pub(crate) fn parse_json(
    json_strings: Box<dyn EngineData>,
    schema: SchemaRef,
) -> DeltaResult<Box<dyn EngineData>> {
    let json_strings: RecordBatch = ArrowEngineData::try_from_engine_data(json_strings)?.into();
    let json_strings = json_strings
        .column(0)
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| {
            Error::generic("Expected json_strings to be a StringArray, found something else")
        })?;
    let schema = Arc::new(ArrowSchema::try_from_kernel(schema.as_ref())?);
    let result = parse_json_impl(json_strings, schema)?;
    Ok(Box::new(ArrowEngineData::new(result)))
}

// Raw arrow implementation of the json parsing. Separate from the public function for testing.
//
// NOTE: This code is really inefficient because arrow lacks the native capability to perform robust
// StringArray -> StructArray JSON parsing. See https://github.com/apache/arrow-rs/issues/6522. If
// that shortcoming gets fixed upstream, this method can simplify or hopefully even disappear.
fn parse_json_impl(json_strings: &StringArray, schema: ArrowSchemaRef) -> DeltaResult<RecordBatch> {
    if json_strings.is_empty() {
        return Ok(RecordBatch::new_empty(schema));
    }

    // Use batch size of 1 to force one record per string input
    let mut decoder = ReaderBuilder::new(schema.clone())
        .with_batch_size(1)
        .build_decoder()?;
    let parse_one = |json_string: Option<&str>| -> DeltaResult<RecordBatch> {
        let mut reader = BufReader::new(json_string.unwrap_or("{}").as_bytes());
        let buf = reader.fill_buf()?;
        let read = buf.len();
        require!(
            decoder.decode(buf)? == read,
            Error::missing_data("Incomplete JSON string")
        );
        let Some(batch) = decoder.flush()? else {
            return Err(Error::missing_data("Expected data"));
        };
        require!(batch.num_rows() == 1, Error::generic("Expected one row"));
        Ok(batch)
    };
    let output: Vec<_> = json_strings.iter().map(parse_one).try_collect()?;
    Ok(concat_batches(&schema, output.iter())?)
}

/// serialize an arrow RecordBatch to a JSON string by appending to a buffer.
// TODO (zach): this should stream data to the JSON writer and output an iterator.
#[internal_api]
pub(crate) fn to_json_bytes(
    data: impl Iterator<Item = DeltaResult<Box<dyn EngineData>>> + Send,
) -> DeltaResult<Vec<u8>> {
    let mut writer = LineDelimitedWriter::new(Vec::new());
    for chunk in data {
        let arrow_data = ArrowEngineData::try_from_engine_data(chunk?)?;
        let record_batch = arrow_data.record_batch();
        writer.write(record_batch)?;
    }
    writer.finish()?;
    Ok(writer.into_inner())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::arrow::array::{
        Array, ArrayRef as ArrowArrayRef, BooleanArray, GenericListArray, Int32Array, Int32Builder,
        MapArray, MapBuilder, StructArray, StructBuilder,
    };
    use crate::arrow::datatypes::{
        DataType as ArrowDataType, Field as ArrowField, Fields, Schema as ArrowSchema,
        SchemaRef as ArrowSchemaRef,
    };
    use crate::arrow::{
        array::AsArray,
        buffer::{OffsetBuffer, ScalarBuffer},
    };

    use crate::schema::{ArrayType, DataType, MapType, StructField, StructType};

    use super::*;

    fn nested_parquet_schema() -> ArrowSchemaRef {
        Arc::new(ArrowSchema::new(vec![
            ArrowField::new("i", ArrowDataType::Int32, false),
            ArrowField::new(
                "nested",
                ArrowDataType::Struct(
                    vec![
                        ArrowField::new("int32", ArrowDataType::Int32, false),
                        ArrowField::new("string", ArrowDataType::Utf8, false),
                    ]
                    .into(),
                ),
                false,
            ),
            ArrowField::new("j", ArrowDataType::Int32, false),
        ]))
    }

    #[test]
    fn test_json_parsing() {
        let requested_schema = Arc::new(ArrowSchema::new(vec![
            ArrowField::new("a", ArrowDataType::Int32, true),
            ArrowField::new("b", ArrowDataType::Utf8, true),
            ArrowField::new("c", ArrowDataType::Int32, true),
        ]));
        let input: Vec<&str> = vec![];
        let result = parse_json_impl(&input.into(), requested_schema.clone()).unwrap();
        assert_eq!(result.num_rows(), 0);

        let input: Vec<Option<&str>> = vec![Some("")];
        let result = parse_json_impl(&input.into(), requested_schema.clone());
        result.expect_err("empty string");

        let input: Vec<Option<&str>> = vec![Some(" \n\t")];
        let result = parse_json_impl(&input.into(), requested_schema.clone());
        result.expect_err("empty string");

        let input: Vec<Option<&str>> = vec![Some(r#""a""#)];
        let result = parse_json_impl(&input.into(), requested_schema.clone());
        result.expect_err("invalid string");

        let input: Vec<Option<&str>> = vec![Some(r#"{ "a": 1"#)];
        let result = parse_json_impl(&input.into(), requested_schema.clone());
        result.expect_err("incomplete object");

        let input: Vec<Option<&str>> = vec![Some("{}{}")];
        let result = parse_json_impl(&input.into(), requested_schema.clone());
        result.expect_err("multiple objects (complete)");

        let input: Vec<Option<&str>> = vec![Some(r#"{} { "a": 1"#)];
        let result = parse_json_impl(&input.into(), requested_schema.clone());
        result.expect_err("multiple objects (partial)");

        let input: Vec<Option<&str>> = vec![Some(r#"{ "a": 1"#), Some(r#", "b"}"#)];
        let result = parse_json_impl(&input.into(), requested_schema.clone());
        result.expect_err("split object");

        let input: Vec<Option<&str>> = vec![None, Some(r#"{"a": 1, "b": "2", "c": 3}"#), None];
        let result = parse_json_impl(&input.into(), requested_schema.clone()).unwrap();
        assert_eq!(result.num_rows(), 3);
        assert_eq!(result.column(0).null_count(), 2);
        assert_eq!(result.column(1).null_count(), 2);
        assert_eq!(result.column(2).null_count(), 2);
    }

    #[test]
    fn simple_mask_indices() {
        let requested_schema = Arc::new(StructType::new([
            StructField::not_null("i", DataType::INTEGER),
            StructField::nullable("s", DataType::STRING),
            StructField::nullable("i2", DataType::INTEGER),
        ]));
        let parquet_schema = Arc::new(ArrowSchema::new(vec![
            ArrowField::new("i", ArrowDataType::Int32, false),
            ArrowField::new("s", ArrowDataType::Utf8, true),
            ArrowField::new("i2", ArrowDataType::Int32, true),
        ]));
        let (mask_indices, reorder_indices) =
            get_requested_indices(&requested_schema, &parquet_schema).unwrap();
        let expect_mask = vec![0, 1, 2];
        let expect_reorder = vec![
            ReorderIndex::identity(0),
            ReorderIndex::identity(1),
            ReorderIndex::identity(2),
        ];
        assert_eq!(mask_indices, expect_mask);
        assert_eq!(reorder_indices, expect_reorder);
    }

    #[test]
    fn test_variant_masks() {
        fn unshredded_variant_parquet_schema() -> ArrowField {
            ArrowField::new(
                "v",
                ArrowDataType::Struct(
                    vec![
                        ArrowField::new("metadata", ArrowDataType::Binary, false),
                        ArrowField::new("value", ArrowDataType::Binary, false),
                    ]
                    .into(),
                ),
                true,
            )
        }
        fn shredded_variant_parquet_schema() -> ArrowField {
            ArrowField::new(
                "v",
                ArrowDataType::Struct(
                    vec![
                        ArrowField::new("metadata", ArrowDataType::Binary, false),
                        ArrowField::new("value", ArrowDataType::Binary, true),
                        ArrowField::new("typed_value", ArrowDataType::Int32, true),
                    ]
                    .into(),
                ),
                true,
            )
        }
        fn incorrect_variant_parquet_schema() -> ArrowField {
            ArrowField::new(
                "v",
                ArrowDataType::Struct(
                    vec![
                        ArrowField::new("field1", ArrowDataType::Binary, false),
                        ArrowField::new("field2", ArrowDataType::Binary, false),
                    ]
                    .into(),
                ),
                true,
            )
        }
        fn scalar_variant_parquet_schema() -> ArrowField {
            ArrowField::new("v", ArrowDataType::Int16, true)
        }
        // Top level variant
        let requested_schema = Arc::new(StructType::new([StructField::nullable(
            "v",
            DataType::unshredded_variant(),
        )]));
        let unshredded_parquet_schema =
            Arc::new(ArrowSchema::new(vec![unshredded_variant_parquet_schema()]));
        let shredded_parquet_schema =
            Arc::new(ArrowSchema::new(vec![shredded_variant_parquet_schema()]));
        let incorrect_parquet_schema =
            Arc::new(ArrowSchema::new(vec![incorrect_variant_parquet_schema()]));
        let scalar_parquet_schema =
            Arc::new(ArrowSchema::new(vec![scalar_variant_parquet_schema()]));
        let result_unshredded =
            get_requested_indices(&requested_schema, &unshredded_parquet_schema);
        assert!(result_unshredded.is_ok());
        let result_shredded = get_requested_indices(&requested_schema, &shredded_parquet_schema);
        assert!(matches!(result_shredded,
            Err(e) if e.to_string().contains("The default engine does not support shredded reads")));
        let result_incorrect = get_requested_indices(&requested_schema, &incorrect_parquet_schema);
        assert!(matches!(result_incorrect,
            Err(e) if e.to_string().contains("The default engine does not support shredded reads")));
        let result_scalar = get_requested_indices(&requested_schema, &scalar_parquet_schema);
        assert!(matches!(result_scalar,
            Err(e) if e.to_string().contains("The default engine does not support shredded reads")));

        // Struct of Variant
        let requested_schema = Arc::new(StructType::new([StructField::nullable(
            "struct_v",
            StructType::new([StructField::nullable("v", DataType::unshredded_variant())]),
        )]));
        let unshredded_parquet_schema = Arc::new(ArrowSchema::new(vec![ArrowField::new(
            "struct_v",
            ArrowDataType::Struct(vec![unshredded_variant_parquet_schema()].into()),
            true,
        )]));
        let shredded_parquet_schema = Arc::new(ArrowSchema::new(vec![ArrowField::new(
            "struct_v",
            ArrowDataType::Struct(vec![shredded_variant_parquet_schema()].into()),
            true,
        )]));
        let result_unshredded =
            get_requested_indices(&requested_schema, &unshredded_parquet_schema);
        let result_shredded = get_requested_indices(&requested_schema, &shredded_parquet_schema);
        assert!(result_unshredded.is_ok());
        assert!(matches!(result_shredded,
            Err(e) if e.to_string().contains("The default engine does not support shredded reads")));
        // Array of Variant
        let requested_schema = Arc::new(StructType::new([StructField::nullable(
            "array_v",
            ArrayType::new(DataType::unshredded_variant(), true),
        )]));
        let unshredded_parquet_schema = Arc::new(ArrowSchema::new(vec![ArrowField::new(
            "array_v",
            ArrowDataType::List(Arc::new(unshredded_variant_parquet_schema())),
            true,
        )]));
        let shredded_parquet_schema = Arc::new(ArrowSchema::new(vec![ArrowField::new(
            "array_v",
            ArrowDataType::List(Arc::new(shredded_variant_parquet_schema())),
            true,
        )]));
        let result_unshredded =
            get_requested_indices(&requested_schema, &unshredded_parquet_schema);
        let result_shredded = get_requested_indices(&requested_schema, &shredded_parquet_schema);
        assert!(result_unshredded.is_ok());
        assert!(matches!(result_shredded,
            Err(e) if e.to_string().contains("The default engine does not support shredded reads")));

        // Map of Variant
        let requested_schema = Arc::new(StructType::new([StructField::nullable(
            "map_v",
            MapType::new(DataType::STRING, DataType::unshredded_variant(), true),
        )]));
        let unshredded_parquet_schema = Arc::new(ArrowSchema::new(vec![ArrowField::new_map(
            "map_v",
            "struc_v",
            ArrowField::new("s", ArrowDataType::Utf8, false),
            unshredded_variant_parquet_schema(),
            false,
            false,
        )]));
        let shredded_parquet_schema = Arc::new(ArrowSchema::new(vec![ArrowField::new_map(
            "map_v",
            "struc_v",
            ArrowField::new("s", ArrowDataType::Utf8, false),
            shredded_variant_parquet_schema(),
            false,
            false,
        )]));
        let result_unshredded =
            get_requested_indices(&requested_schema, &unshredded_parquet_schema);
        let result_shredded = get_requested_indices(&requested_schema, &shredded_parquet_schema);
        assert!(result_unshredded.is_ok());
        assert!(matches!(result_shredded,
            Err(e) if e.to_string().contains("The default engine does not support shredded reads")));
    }

    #[test]
    fn ensure_data_types_fails_correctly() {
        let requested_schema = Arc::new(StructType::new([
            StructField::not_null("i", DataType::INTEGER),
            StructField::nullable("s", DataType::INTEGER),
        ]));
        let parquet_schema = Arc::new(ArrowSchema::new(vec![
            ArrowField::new("i", ArrowDataType::Int32, false),
            ArrowField::new("s", ArrowDataType::Utf8, true),
        ]));
        let res = get_requested_indices(&requested_schema, &parquet_schema);
        assert!(res.is_err());

        let requested_schema = Arc::new(StructType::new([
            StructField::not_null("i", DataType::INTEGER),
            StructField::nullable("s", DataType::STRING),
        ]));
        let parquet_schema = Arc::new(ArrowSchema::new(vec![
            ArrowField::new("i", ArrowDataType::Int32, false),
            ArrowField::new("s", ArrowDataType::Int32, true),
        ]));
        let res = get_requested_indices(&requested_schema, &parquet_schema);
        assert!(res.is_err());
    }

    #[test]
    fn mask_with_map() {
        let requested_schema = Arc::new(StructType::new([StructField::not_null(
            "map",
            MapType::new(DataType::INTEGER, DataType::STRING, false),
        )]));
        let parquet_schema = Arc::new(ArrowSchema::new(vec![ArrowField::new_map(
            "map",
            "entries",
            ArrowField::new("i", ArrowDataType::Int32, false),
            ArrowField::new("s", ArrowDataType::Utf8, false),
            false,
            false,
        )]));
        let (mask_indices, reorder_indices) =
            get_requested_indices(&requested_schema, &parquet_schema).unwrap();
        let expect_mask = vec![0, 1];
        let expect_reorder = vec![ReorderIndex::identity(0)];
        assert_eq!(mask_indices, expect_mask);
        assert_eq!(reorder_indices, expect_reorder);
    }

    #[test]
    fn simple_reorder_indices() {
        let requested_schema = Arc::new(StructType::new([
            StructField::not_null("i", DataType::INTEGER),
            StructField::nullable("s", DataType::STRING),
            StructField::nullable("i2", DataType::INTEGER),
        ]));
        let parquet_schema = Arc::new(ArrowSchema::new(vec![
            ArrowField::new("i2", ArrowDataType::Int32, true),
            ArrowField::new("i", ArrowDataType::Int32, false),
            ArrowField::new("s", ArrowDataType::Utf8, true),
        ]));
        let (mask_indices, reorder_indices) =
            get_requested_indices(&requested_schema, &parquet_schema).unwrap();
        let expect_mask = vec![0, 1, 2];
        let expect_reorder = vec![
            ReorderIndex::identity(2),
            ReorderIndex::identity(0),
            ReorderIndex::identity(1),
        ];
        assert_eq!(mask_indices, expect_mask);
        assert_eq!(reorder_indices, expect_reorder);
    }

    #[test]
    fn simple_nullable_field_missing() {
        let requested_schema = Arc::new(StructType::new([
            StructField::not_null("i", DataType::INTEGER),
            StructField::nullable("s", DataType::STRING),
            StructField::nullable("i2", DataType::INTEGER),
        ]));
        let parquet_schema = Arc::new(ArrowSchema::new(vec![
            ArrowField::new("i", ArrowDataType::Int32, false),
            ArrowField::new("i2", ArrowDataType::Int32, true),
        ]));
        let (mask_indices, reorder_indices) =
            get_requested_indices(&requested_schema, &parquet_schema).unwrap();
        let expect_mask = vec![0, 1];
        let expect_reorder = vec![
            ReorderIndex::identity(0),
            ReorderIndex::identity(2),
            ReorderIndex::missing(1, Arc::new(ArrowField::new("s", ArrowDataType::Utf8, true))),
        ];
        assert_eq!(mask_indices, expect_mask);
        assert_eq!(reorder_indices, expect_reorder);
    }

    #[test]
    fn nested_indices() {
        let requested_schema = Arc::new(StructType::new([
            StructField::not_null("i", DataType::INTEGER),
            StructField::not_null(
                "nested",
                StructType::new([
                    StructField::not_null("int32", DataType::INTEGER),
                    StructField::not_null("string", DataType::STRING),
                ]),
            ),
            StructField::not_null("j", DataType::INTEGER),
        ]));
        let parquet_schema = nested_parquet_schema();
        let (mask_indices, reorder_indices) =
            get_requested_indices(&requested_schema, &parquet_schema).unwrap();
        let expect_mask = vec![0, 1, 2, 3];
        let expect_reorder = vec![
            ReorderIndex::identity(0),
            ReorderIndex::nested(
                1,
                vec![ReorderIndex::identity(0), ReorderIndex::identity(1)],
            ),
            ReorderIndex::identity(2),
        ];
        assert_eq!(mask_indices, expect_mask);
        assert_eq!(reorder_indices, expect_reorder);
    }

    #[test]
    fn nested_indices_reorder() {
        let requested_schema = Arc::new(StructType::new([
            StructField::not_null(
                "nested",
                StructType::new([
                    StructField::not_null("string", DataType::STRING),
                    StructField::not_null("int32", DataType::INTEGER),
                ]),
            ),
            StructField::not_null("j", DataType::INTEGER),
            StructField::not_null("i", DataType::INTEGER),
        ]));
        let parquet_schema = nested_parquet_schema();
        let (mask_indices, reorder_indices) =
            get_requested_indices(&requested_schema, &parquet_schema).unwrap();
        let expect_mask = vec![0, 1, 2, 3];
        let expect_reorder = vec![
            ReorderIndex::identity(2),
            ReorderIndex::nested(
                0,
                vec![ReorderIndex::identity(1), ReorderIndex::identity(0)],
            ),
            ReorderIndex::identity(1),
        ];
        assert_eq!(mask_indices, expect_mask);
        assert_eq!(reorder_indices, expect_reorder);
    }

    #[test]
    fn nested_indices_mask_inner() {
        let requested_schema = Arc::new(StructType::new([
            StructField::not_null("i", DataType::INTEGER),
            StructField::not_null(
                "nested",
                StructType::new([StructField::not_null("int32", DataType::INTEGER)]),
            ),
            StructField::not_null("j", DataType::INTEGER),
        ]));
        let parquet_schema = nested_parquet_schema();
        let (mask_indices, reorder_indices) =
            get_requested_indices(&requested_schema, &parquet_schema).unwrap();
        let expect_mask = vec![0, 1, 3];
        let expect_reorder = vec![
            ReorderIndex::identity(0),
            ReorderIndex::nested(1, vec![ReorderIndex::identity(0)]),
            ReorderIndex::identity(2),
        ];
        assert_eq!(mask_indices, expect_mask);
        assert_eq!(reorder_indices, expect_reorder);
    }

    #[test]
    fn simple_list_mask() {
        let requested_schema = Arc::new(StructType::new([
            StructField::not_null("i", DataType::INTEGER),
            StructField::not_null("list", ArrayType::new(DataType::INTEGER, false)),
            StructField::not_null("j", DataType::INTEGER),
        ]));
        let parquet_schema = Arc::new(ArrowSchema::new(vec![
            ArrowField::new("i", ArrowDataType::Int32, false),
            ArrowField::new(
                "list",
                ArrowDataType::List(Arc::new(ArrowField::new(
                    "nested",
                    ArrowDataType::Int32,
                    false,
                ))),
                false,
            ),
            ArrowField::new("j", ArrowDataType::Int32, false),
        ]));
        let (mask_indices, reorder_indices) =
            get_requested_indices(&requested_schema, &parquet_schema).unwrap();
        let expect_mask = vec![0, 1, 2];
        let expect_reorder = vec![
            ReorderIndex::identity(0),
            ReorderIndex::identity(1),
            ReorderIndex::identity(2),
        ];
        assert_eq!(mask_indices, expect_mask);
        assert_eq!(reorder_indices, expect_reorder);
    }

    #[test]
    fn list_skip_earlier_element() {
        let requested_schema = Arc::new(StructType::new([StructField::not_null(
            "list",
            ArrayType::new(DataType::INTEGER, false),
        )]));
        let parquet_schema = Arc::new(ArrowSchema::new(vec![
            ArrowField::new("i", ArrowDataType::Int32, false),
            ArrowField::new(
                "list",
                ArrowDataType::List(Arc::new(ArrowField::new(
                    "nested",
                    ArrowDataType::Int32,
                    false,
                ))),
                false,
            ),
        ]));
        let (mask_indices, reorder_indices) =
            get_requested_indices(&requested_schema, &parquet_schema).unwrap();
        let expect_mask = vec![1];
        let expect_reorder = vec![ReorderIndex::identity(0)];
        assert_eq!(mask_indices, expect_mask);
        assert_eq!(reorder_indices, expect_reorder);
    }

    #[test]
    fn nested_indices_list() {
        let requested_schema = Arc::new(StructType::new([
            StructField::not_null("i", DataType::INTEGER),
            StructField::not_null(
                "list",
                ArrayType::new(
                    StructType::new([
                        StructField::not_null("int32", DataType::INTEGER),
                        StructField::not_null("string", DataType::STRING),
                    ])
                    .into(),
                    false,
                ),
            ),
            StructField::not_null("j", DataType::INTEGER),
        ]));
        let parquet_schema = Arc::new(ArrowSchema::new(vec![
            ArrowField::new("i", ArrowDataType::Int32, false),
            ArrowField::new(
                "list",
                ArrowDataType::List(Arc::new(ArrowField::new(
                    "nested",
                    ArrowDataType::Struct(
                        vec![
                            ArrowField::new("int32", ArrowDataType::Int32, false),
                            ArrowField::new("string", ArrowDataType::Utf8, false),
                        ]
                        .into(),
                    ),
                    false,
                ))),
                false,
            ),
            ArrowField::new("j", ArrowDataType::Int32, false),
        ]));
        let (mask_indices, reorder_indices) =
            get_requested_indices(&requested_schema, &parquet_schema).unwrap();
        let expect_mask = vec![0, 1, 2, 3];
        let expect_reorder = vec![
            ReorderIndex::identity(0),
            ReorderIndex::nested(
                1,
                vec![ReorderIndex::identity(0), ReorderIndex::identity(1)],
            ),
            ReorderIndex::identity(2),
        ];
        assert_eq!(mask_indices, expect_mask);
        assert_eq!(reorder_indices, expect_reorder);
    }

    #[test]
    fn nested_indices_unselected_list() {
        let requested_schema = Arc::new(StructType::new([
            StructField::not_null("i", DataType::INTEGER),
            StructField::not_null("j", DataType::INTEGER),
        ]));
        let parquet_schema = Arc::new(ArrowSchema::new(vec![
            ArrowField::new("i", ArrowDataType::Int32, false),
            ArrowField::new(
                "list",
                ArrowDataType::List(Arc::new(ArrowField::new(
                    "nested",
                    ArrowDataType::Struct(
                        vec![
                            ArrowField::new("int32", ArrowDataType::Int32, false),
                            ArrowField::new("string", ArrowDataType::Utf8, false),
                        ]
                        .into(),
                    ),
                    false,
                ))),
                false,
            ),
            ArrowField::new("j", ArrowDataType::Int32, false),
        ]));
        let (mask_indices, reorder_indices) =
            get_requested_indices(&requested_schema, &parquet_schema).unwrap();
        let expect_mask = vec![0, 3];
        let expect_reorder = vec![ReorderIndex::identity(0), ReorderIndex::identity(1)];
        assert_eq!(mask_indices, expect_mask);
        assert_eq!(reorder_indices, expect_reorder);
    }

    #[test]
    fn nested_indices_list_mask_inner() {
        let requested_schema = Arc::new(StructType::new([
            StructField::not_null("i", DataType::INTEGER),
            StructField::not_null(
                "list",
                ArrayType::new(
                    StructType::new([StructField::not_null("int32", DataType::INTEGER)]).into(),
                    false,
                ),
            ),
            StructField::not_null("j", DataType::INTEGER),
        ]));
        let parquet_schema = Arc::new(ArrowSchema::new(vec![
            ArrowField::new("i", ArrowDataType::Int32, false),
            ArrowField::new(
                "list",
                ArrowDataType::List(Arc::new(ArrowField::new(
                    "nested",
                    ArrowDataType::Struct(
                        vec![
                            ArrowField::new("int32", ArrowDataType::Int32, false),
                            ArrowField::new("string", ArrowDataType::Utf8, false),
                        ]
                        .into(),
                    ),
                    false,
                ))),
                false,
            ),
            ArrowField::new("j", ArrowDataType::Int32, false),
        ]));
        let (mask_indices, reorder_indices) =
            get_requested_indices(&requested_schema, &parquet_schema).unwrap();
        let expect_mask = vec![0, 1, 3];
        let expect_reorder = vec![
            ReorderIndex::identity(0),
            ReorderIndex::nested(1, vec![ReorderIndex::identity(0)]),
            ReorderIndex::identity(2),
        ];
        assert_eq!(mask_indices, expect_mask);
        assert_eq!(reorder_indices, expect_reorder);
    }

    #[test]
    fn nested_indices_list_mask_inner_reorder() {
        let requested_schema = Arc::new(StructType::new([
            StructField::not_null("i", DataType::INTEGER),
            StructField::not_null(
                "list",
                ArrayType::new(
                    StructType::new([
                        StructField::not_null("string", DataType::STRING),
                        StructField::not_null("int2", DataType::INTEGER),
                    ])
                    .into(),
                    false,
                ),
            ),
            StructField::not_null("j", DataType::INTEGER),
        ]));
        let parquet_schema = Arc::new(ArrowSchema::new(vec![
            ArrowField::new("i", ArrowDataType::Int32, false), // field 0
            ArrowField::new(
                "list",
                ArrowDataType::List(Arc::new(ArrowField::new(
                    "nested",
                    ArrowDataType::Struct(
                        vec![
                            ArrowField::new("int1", ArrowDataType::Int32, false), // field 1
                            ArrowField::new("int2", ArrowDataType::Int32, false), // field 2
                            ArrowField::new("string", ArrowDataType::Utf8, false), // field 3
                        ]
                        .into(),
                    ),
                    false,
                ))),
                false,
            ),
            ArrowField::new("j", ArrowDataType::Int32, false), // field 4
        ]));
        let (mask_indices, reorder_indices) =
            get_requested_indices(&requested_schema, &parquet_schema).unwrap();
        let expect_mask = vec![0, 2, 3, 4];
        let expect_reorder = vec![
            ReorderIndex::identity(0),
            ReorderIndex::nested(
                1,
                vec![ReorderIndex::identity(1), ReorderIndex::identity(0)],
            ),
            ReorderIndex::identity(2),
        ];
        assert_eq!(mask_indices, expect_mask);
        assert_eq!(reorder_indices, expect_reorder);
    }

    #[test]
    fn skipped_struct() {
        let requested_schema = Arc::new(StructType::new([
            StructField::not_null("i", DataType::INTEGER),
            StructField::not_null(
                "nested",
                StructType::new([
                    StructField::not_null("int32", DataType::INTEGER),
                    StructField::not_null("string", DataType::STRING),
                ]),
            ),
            StructField::not_null("j", DataType::INTEGER),
        ]));
        let parquet_schema = Arc::new(ArrowSchema::new(vec![
            ArrowField::new(
                "skipped",
                ArrowDataType::Struct(
                    vec![
                        ArrowField::new("int32", ArrowDataType::Int32, false),
                        ArrowField::new("string", ArrowDataType::Utf8, false),
                    ]
                    .into(),
                ),
                false,
            ),
            ArrowField::new("j", ArrowDataType::Int32, false),
            ArrowField::new(
                "nested",
                ArrowDataType::Struct(
                    vec![
                        ArrowField::new("int32", ArrowDataType::Int32, false),
                        ArrowField::new("string", ArrowDataType::Utf8, false),
                    ]
                    .into(),
                ),
                false,
            ),
            ArrowField::new("i", ArrowDataType::Int32, false),
        ]));
        let (mask_indices, reorder_indices) =
            get_requested_indices(&requested_schema, &parquet_schema).unwrap();
        let expect_mask = vec![2, 3, 4, 5];
        let expect_reorder = vec![
            ReorderIndex::identity(2),
            ReorderIndex::nested(
                1,
                vec![ReorderIndex::identity(0), ReorderIndex::identity(1)],
            ),
            ReorderIndex::identity(0),
        ];
        assert_eq!(mask_indices, expect_mask);
        assert_eq!(reorder_indices, expect_reorder);
    }

    #[test]
    fn reorder_map_with_structs() {
        let requested_schema = Arc::new(StructType::new([
            StructField::not_null("i", DataType::INTEGER),
            StructField::not_null(
                "map",
                MapType::new(
                    StructType::new([
                        StructField::not_null("k1", DataType::STRING),
                        StructField::not_null("k2", DataType::STRING),
                    ]),
                    StructType::new([
                        StructField::not_null("v2", DataType::STRING),
                        StructField::not_null("v1", DataType::STRING),
                    ]),
                    false,
                ),
            ),
        ]));
        let parquet_schema = Arc::new(ArrowSchema::new(vec![
            ArrowField::new("i", ArrowDataType::Int32, false),
            ArrowField::new_map(
                "map",
                "entries",
                ArrowField::new(
                    "i",
                    ArrowDataType::Struct(
                        vec![
                            ArrowField::new("k1", ArrowDataType::Utf8, false),
                            ArrowField::new("k2", ArrowDataType::Utf8, false),
                        ]
                        .into(),
                    ),
                    false,
                ),
                ArrowField::new(
                    "v",
                    ArrowDataType::Struct(
                        vec![
                            ArrowField::new("v1", ArrowDataType::Utf8, false),
                            ArrowField::new("v2", ArrowDataType::Utf8, false),
                        ]
                        .into(),
                    ),
                    false,
                ),
                false,
                false,
            ),
        ]));
        let (mask_indices, reorder_indices) =
            get_requested_indices(&requested_schema, &parquet_schema).unwrap();
        let expect_mask = vec![0, 1, 2, 3, 4];
        let expect_reorder = vec![
            ReorderIndex::identity(0),
            ReorderIndex::nested(
                1,
                vec![
                    ReorderIndex::identity(0), // key does not need re-ordering
                    ReorderIndex::nested(
                        1,
                        vec![ReorderIndex::identity(1), ReorderIndex::identity(0)],
                    ),
                ],
            ),
        ];
        assert_eq!(mask_indices, expect_mask);
        assert_eq!(reorder_indices, expect_reorder);
    }

    fn make_struct_array() -> StructArray {
        let boolean = Arc::new(BooleanArray::from(vec![false, false, true, true]));
        let int = Arc::new(Int32Array::from(vec![42, 28, 19, 31]));
        StructArray::from(vec![
            (
                Arc::new(ArrowField::new("b", ArrowDataType::Boolean, false)),
                boolean.clone() as ArrowArrayRef,
            ),
            (
                Arc::new(ArrowField::new("c", ArrowDataType::Int32, false)),
                int.clone() as ArrowArrayRef,
            ),
        ])
    }

    #[test]
    fn simple_reorder_struct() {
        let arry = make_struct_array();
        let reorder = vec![ReorderIndex::identity(1), ReorderIndex::identity(0)];
        let ordered = reorder_struct_array(arry, &reorder).unwrap();
        assert_eq!(ordered.column_names(), vec!["c", "b"]);
    }

    #[test]
    fn nested_reorder_struct() {
        let arry1 = Arc::new(make_struct_array());
        let arry2 = Arc::new(make_struct_array());
        let fields: Fields = vec![
            Arc::new(ArrowField::new("b", ArrowDataType::Boolean, false)),
            Arc::new(ArrowField::new("c", ArrowDataType::Int32, false)),
        ]
        .into();
        let nested = StructArray::from(vec![
            (
                Arc::new(ArrowField::new(
                    "struct1",
                    ArrowDataType::Struct(fields.clone()),
                    false,
                )),
                arry1 as ArrowArrayRef,
            ),
            (
                Arc::new(ArrowField::new(
                    "struct2",
                    ArrowDataType::Struct(fields),
                    false,
                )),
                arry2 as ArrowArrayRef,
            ),
        ]);
        let reorder = vec![
            ReorderIndex::nested(
                1,
                vec![ReorderIndex::identity(1), ReorderIndex::identity(0)],
            ),
            ReorderIndex::nested(
                0,
                vec![
                    ReorderIndex::identity(0),
                    ReorderIndex::identity(1),
                    ReorderIndex::missing(
                        2,
                        Arc::new(ArrowField::new("s", ArrowDataType::Utf8, true)),
                    ),
                ],
            ),
        ];
        let ordered = reorder_struct_array(nested, &reorder).unwrap();
        assert_eq!(ordered.column_names(), vec!["struct2", "struct1"]);
        let ordered_s2 = ordered.column(0).as_struct();
        assert_eq!(ordered_s2.column_names(), vec!["b", "c", "s"]);
        let ordered_s1 = ordered.column(1).as_struct();
        assert_eq!(ordered_s1.column_names(), vec!["c", "b"]);
    }

    #[test]
    fn reorder_list_of_struct() {
        let boolean = Arc::new(BooleanArray::from(vec![
            false, false, true, true, false, true,
        ]));
        let int = Arc::new(Int32Array::from(vec![42, 28, 19, 31, 0, 3]));
        let list_sa = StructArray::from(vec![
            (
                Arc::new(ArrowField::new("b", ArrowDataType::Boolean, false)),
                boolean.clone() as ArrowArrayRef,
            ),
            (
                Arc::new(ArrowField::new("c", ArrowDataType::Int32, false)),
                int.clone() as ArrowArrayRef,
            ),
        ]);
        let offsets = OffsetBuffer::new(ScalarBuffer::from(vec![0, 3, 6]));
        let list_field = ArrowField::new("item", list_sa.data_type().clone(), false);
        let list = Arc::new(GenericListArray::new(
            Arc::new(list_field),
            offsets,
            Arc::new(list_sa),
            None,
        ));
        let fields: Fields = vec![
            Arc::new(ArrowField::new("b", ArrowDataType::Boolean, false)),
            Arc::new(ArrowField::new("c", ArrowDataType::Int32, false)),
        ]
        .into();
        let list_dt = Arc::new(ArrowField::new(
            "list",
            ArrowDataType::new_list(ArrowDataType::Struct(fields), false),
            false,
        ));
        let struct_array = StructArray::from(vec![(list_dt, list as ArrowArrayRef)]);
        let reorder = vec![ReorderIndex::nested(
            0,
            vec![ReorderIndex::identity(1), ReorderIndex::identity(0)],
        )];
        let ordered = reorder_struct_array(struct_array, &reorder).unwrap();
        let ordered_list_col = ordered.column(0).as_list::<i32>();
        for i in 0..ordered_list_col.len() {
            let array_item = ordered_list_col.value(i);
            let struct_item = array_item.as_struct();
            assert_eq!(struct_item.column_names(), vec!["c", "b"]);
        }
    }

    // boy howdy this is more complicated than expected
    fn build_arrow_map() -> MapArray {
        let key_struct_builder = StructBuilder::from_fields(
            Fields::from(vec![
                ArrowField::new("k1", ArrowDataType::Int32, false),
                ArrowField::new("k2", ArrowDataType::Int32, false),
            ]),
            1,
        );
        let value_struct_builder = StructBuilder::from_fields(
            Fields::from(vec![
                ArrowField::new("v1", ArrowDataType::Int32, false),
                ArrowField::new("v2", ArrowDataType::Int32, false),
            ]),
            1,
        );
        let mut map_builder = MapBuilder::new(None, key_struct_builder, value_struct_builder);

        let (key_builder, value_builder) = map_builder.entries();
        let key_k1_builder = key_builder.field_builder::<Int32Builder>(0).unwrap();
        key_k1_builder.append_value(1);
        let key_k2_builder = key_builder.field_builder::<Int32Builder>(1).unwrap();
        key_k2_builder.append_value(2);
        key_builder.append(true);

        let value_v1_builder = value_builder.field_builder::<Int32Builder>(0).unwrap();
        value_v1_builder.append_value(1);
        let value_v2_builder = value_builder.field_builder::<Int32Builder>(1).unwrap();
        value_v2_builder.append_value(2);
        value_builder.append(true);
        map_builder.append(true).unwrap();
        map_builder.finish()
    }

    #[test]
    fn reorder_map_of_struct() {
        let int_array = Arc::new(Int32Array::from(vec![42]));
        let int_dt = Arc::new(ArrowField::new("i", int_array.data_type().clone(), false));
        let map_array = Arc::new(build_arrow_map());
        let map_dt = Arc::new(ArrowField::new("map", map_array.data_type().clone(), false));
        let struct_array = StructArray::from(vec![
            (int_dt, int_array as ArrowArrayRef),
            (map_dt, map_array as ArrowArrayRef),
        ]);
        let reorder = vec![
            ReorderIndex::identity(1),
            ReorderIndex::nested(
                0,
                vec![
                    ReorderIndex::identity(0),
                    ReorderIndex::nested(
                        1,
                        vec![ReorderIndex::identity(1), ReorderIndex::identity(0)],
                    ),
                ],
            ),
        ];
        let ordered = reorder_struct_array(struct_array, &reorder).unwrap();
        assert_eq!(ordered.column_names(), vec!["map", "i"]);
        if let ArrowDataType::Map(field, _) = ordered.column(0).data_type() {
            if let ArrowDataType::Struct(fields) = field.data_type() {
                fn assert_col_order(field: &ArrowField, expected: Vec<&str>) {
                    if let ArrowDataType::Struct(fields) = field.data_type() {
                        let names: Vec<&str> =
                            fields.iter().map(|field| field.name().as_str()).collect();
                        assert_eq!(names, expected);
                    } else {
                        panic!("Expected struct field");
                    }
                }
                assert_col_order(&fields[0], vec!["k1", "k2"]);
                assert_col_order(&fields[1], vec!["v2", "v1"]);
            } else {
                panic!("Inner field should have been a struct");
            }
        } else {
            panic!("Column 0 should have been a map");
        }
    }

    #[test]
    fn no_matches() {
        let requested_schema = Arc::new(StructType::new([
            StructField::nullable("s", DataType::STRING),
            StructField::nullable("i2", DataType::INTEGER),
        ]));
        let nots_field = ArrowField::new("NOTs", ArrowDataType::Utf8, true);
        let noti2_field = ArrowField::new("NOTi2", ArrowDataType::Int32, true);
        let parquet_schema = Arc::new(ArrowSchema::new(vec![
            nots_field.clone(),
            noti2_field.clone(),
        ]));
        let (mask_indices, reorder_indices) =
            get_requested_indices(&requested_schema, &parquet_schema).unwrap();
        let expect_mask: Vec<usize> = vec![];
        let expect_reorder = vec![
            ReorderIndex::missing(0, nots_field.with_name("s").into()),
            ReorderIndex::missing(1, noti2_field.with_name("i2").into()),
        ];
        assert_eq!(mask_indices, expect_mask);
        assert_eq!(reorder_indices, expect_reorder);
    }

    #[test]
    fn empty_requested_schema() {
        let requested_schema = Arc::new(StructType::new([]));
        let parquet_schema = Arc::new(ArrowSchema::new(vec![
            ArrowField::new("i", ArrowDataType::Int32, false),
            ArrowField::new("s", ArrowDataType::Utf8, true),
            ArrowField::new("i2", ArrowDataType::Int32, true),
        ]));
        let (mask_indices, reorder_indices) =
            get_requested_indices(&requested_schema, &parquet_schema).unwrap();
        let expect_mask: Vec<usize> = vec![];
        let expect_reorder = vec![];
        assert_eq!(mask_indices, expect_mask);
        assert_eq!(reorder_indices, expect_reorder);
    }

    #[test]
    fn test_write_json() -> DeltaResult<()> {
        let schema = Arc::new(ArrowSchema::new(vec![ArrowField::new(
            "string",
            ArrowDataType::Utf8,
            true,
        )]));
        let data = RecordBatch::try_new(
            schema.clone(),
            vec![Arc::new(StringArray::from(vec!["string1", "string2"]))],
        )?;
        let data: Box<dyn EngineData> = Box::new(ArrowEngineData::new(data));
        let json = to_json_bytes(Box::new(std::iter::once(Ok(data))))?;
        assert_eq!(
            json,
            "{\"string\":\"string1\"}\n{\"string\":\"string2\"}\n".as_bytes()
        );
        Ok(())
    }

    #[test]
    fn test_arrow_broken_nested_null_masks() {
        use crate::arrow::datatypes::{DataType, Field, Fields, Schema};
        use crate::engine::arrow_utils::fix_nested_null_masks;
        use crate::parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

        // Parse some JSON into a nested schema
        let schema = Arc::new(Schema::new(vec![Field::new(
            "outer",
            DataType::Struct(Fields::from(vec![
                Field::new(
                    "inner_nullable",
                    DataType::Struct(Fields::from(vec![
                        Field::new("leaf_non_null", DataType::Int32, false),
                        Field::new("leaf_nullable", DataType::Int32, true),
                    ])),
                    true,
                ),
                Field::new(
                    "inner_non_null",
                    DataType::Struct(Fields::from(vec![
                        Field::new("leaf_non_null", DataType::Int32, false),
                        Field::new("leaf_nullable", DataType::Int32, true),
                    ])),
                    false,
                ),
            ])),
            true,
        )]));
        let json_string = r#"
{ }
{ "outer" : { "inner_non_null" : { "leaf_non_null" : 1 } } }
{ "outer" : { "inner_non_null" : { "leaf_non_null" : 2, "leaf_nullable" : 3 } } }
{ "outer" : { "inner_non_null" : { "leaf_non_null" : 4 }, "inner_nullable" : { "leaf_non_null" : 5 } } }
{ "outer" : { "inner_non_null" : { "leaf_non_null" : 6 }, "inner_nullable" : { "leaf_non_null" : 7, "leaf_nullable": 8 } } }
"#;
        let batch1 = crate::arrow::json::ReaderBuilder::new(schema.clone())
            .build(json_string.as_bytes())
            .unwrap()
            .next()
            .unwrap()
            .unwrap();

        macro_rules! assert_nulls {
            ( $column: expr, $nulls: expr ) => {
                assert_eq!($column.nulls().unwrap(), &NullBuffer::from(&$nulls[..]));
            };
        }

        // If any of these tests ever fail, it means the arrow JSON reader started producing
        // incomplete nested NULL masks. If that happens, we need to update all JSON reads to call
        // `fix_nested_null_masks`.
        let outer_1 = batch1.column(0).as_struct();
        assert_nulls!(outer_1, [false, true, true, true, true]);
        let inner_nullable_1 = outer_1.column(0).as_struct();
        assert_nulls!(inner_nullable_1, [false, false, false, true, true]);
        let nullable_leaf_non_null_1 = inner_nullable_1.column(0);
        assert_nulls!(nullable_leaf_non_null_1, [false, false, false, true, true]);
        let nullable_leaf_nullable_1 = inner_nullable_1.column(1);
        assert_nulls!(nullable_leaf_nullable_1, [false, false, false, false, true]);
        let inner_non_null_1 = outer_1.column(1).as_struct();
        assert_nulls!(inner_non_null_1, [false, true, true, true, true]);
        let non_null_leaf_non_null_1 = inner_non_null_1.column(0);
        assert_nulls!(non_null_leaf_non_null_1, [false, true, true, true, true]);
        let non_null_leaf_nullable_1 = inner_non_null_1.column(1);
        assert_nulls!(non_null_leaf_nullable_1, [false, false, true, false, false]);

        // Write the batch to a parquet file and read it back
        let mut buffer = vec![];
        let mut writer =
            crate::parquet::arrow::ArrowWriter::try_new(&mut buffer, schema.clone(), None).unwrap();
        writer.write(&batch1).unwrap();
        writer.close().unwrap(); // writer must be closed to write footer
        let batch2 = ParquetRecordBatchReaderBuilder::try_new(bytes::Bytes::from(buffer))
            .unwrap()
            .build()
            .unwrap()
            .next()
            .unwrap()
            .unwrap();

        // Starting from arrow-53.3, the parquet reader started returning broken nested NULL masks.
        let batch2 = RecordBatch::from(fix_nested_null_masks(batch2.into()));

        // Verify the data survived the round trip
        let outer_2 = batch2.column(0).as_struct();
        assert_eq!(outer_2, outer_1);
        let inner_nullable_2 = outer_2.column(0).as_struct();
        assert_eq!(inner_nullable_2, inner_nullable_1);
        let nullable_leaf_non_null_2 = inner_nullable_2.column(0);
        assert_eq!(nullable_leaf_non_null_2, nullable_leaf_non_null_1);
        let nullable_leaf_nullable_2 = inner_nullable_2.column(1);
        assert_eq!(nullable_leaf_nullable_2, nullable_leaf_nullable_1);
        let inner_non_null_2 = outer_2.column(1).as_struct();
        assert_eq!(inner_non_null_2, inner_non_null_1);
        let non_null_leaf_non_null_2 = inner_non_null_2.column(0);
        assert_eq!(non_null_leaf_non_null_2, non_null_leaf_non_null_1);
        let non_null_leaf_nullable_2 = inner_non_null_2.column(1);
        assert_eq!(non_null_leaf_nullable_2, non_null_leaf_nullable_1);
    }
}
