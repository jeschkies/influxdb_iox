use std::{borrow::Cow, collections::BTreeMap};

use hashbrown::{hash_map, HashMap};
use itertools::Itertools;

use crate::column::{
    cmp::Operator, AggregateResult, AggregateType, Column, EncodedValues, OwnedValue, RowIDs,
    RowIDsOption, Scalar, Value, Values, ValuesIterator,
};

/// The name used for a timestamp column.
pub const TIME_COLUMN_NAME: &str = data_types::TIME_COLUMN_NAME;

/// A `RowGroup` is an immutable horizontal chunk of a single `Table`. By
/// definition it has the same schema as all the other read groups in the table.
/// All the columns within the `RowGroup` must have the same number of logical
/// rows.
pub struct RowGroup {
    meta: MetaData,

    columns: Vec<Column>,
    all_columns_by_name: BTreeMap<String, usize>,
    tag_columns_by_name: BTreeMap<String, usize>,
    field_columns_by_name: BTreeMap<String, usize>,
    time_column: usize,
}

impl RowGroup {
    pub fn new(rows: u32, columns: BTreeMap<String, ColumnType>) -> Self {
        let mut meta = MetaData {
            rows,
            ..MetaData::default()
        };

        let mut all_columns = vec![];
        let mut all_columns_by_name = BTreeMap::new();
        let mut tag_columns_by_name = BTreeMap::new();
        let mut field_columns_by_name = BTreeMap::new();
        let mut time_column = None;

        for (name, ct) in columns {
            meta.size += ct.size();
            match ct {
                ColumnType::Tag(c) => {
                    assert_eq!(c.num_rows(), rows);

                    meta.column_ranges
                        .insert(name.clone(), c.column_range().unwrap());
                    all_columns_by_name.insert(name.clone(), all_columns.len());
                    tag_columns_by_name.insert(name, all_columns.len());
                    all_columns.push(c);
                }
                ColumnType::Field(c) => {
                    assert_eq!(c.num_rows(), rows);

                    meta.column_ranges
                        .insert(name.clone(), c.column_range().unwrap());
                    all_columns_by_name.insert(name.clone(), all_columns.len());
                    field_columns_by_name.insert(name, all_columns.len());
                    all_columns.push(c);
                }
                ColumnType::Time(c) => {
                    assert_eq!(c.num_rows(), rows);

                    meta.time_range = match c.column_range() {
                        None => panic!("time column must have non-null value"),
                        Some((
                            OwnedValue::Scalar(Scalar::I64(min)),
                            OwnedValue::Scalar(Scalar::I64(max)),
                        )) => (min, max),
                        Some((_, _)) => unreachable!("unexpected types for time range"),
                    };

                    meta.column_ranges
                        .insert(name.clone(), c.column_range().unwrap());

                    all_columns_by_name.insert(name.clone(), all_columns.len());
                    time_column = Some(all_columns.len());
                    all_columns.push(c);
                }
            }
        }

        Self {
            meta,
            columns: all_columns,
            all_columns_by_name,
            tag_columns_by_name,
            field_columns_by_name,
            time_column: time_column.unwrap(),
        }
    }

    /// The total size in bytes of the read group
    pub fn size(&self) -> u64 {
        self.meta.size
    }

    /// The number of rows in the `RowGroup` (all columns have the same number
    /// of rows).
    pub fn rows(&self) -> u32 {
        self.meta.rows
    }

    /// The ranges on each column in the `RowGroup`.
    pub fn column_ranges(&self) -> &BTreeMap<String, (OwnedValue, OwnedValue)> {
        &self.meta.column_ranges
    }

    // Returns a reference to a column from the column name.
    //
    // It is the caller's responsibility to ensure the column exists in the read
    // group. Panics if the column doesn't exist.
    fn column_by_name(&self, name: ColumnName<'_>) -> &Column {
        &self.columns[*self.all_columns_by_name.get(name).unwrap()]
    }

    // Takes a `ColumnName`, looks up that column in the `RowGroup`, and
    // returns a reference to that column's name owned by the `RowGroup` along
    // with a reference to the column itself. The returned column name will have
    // the lifetime of `self`, not the lifetime of the input.
    fn column_name_and_column(&self, name: ColumnName<'_>) -> (&str, &Column) {
        let (column_name, column_index) = self.all_columns_by_name.get_key_value(name).unwrap();
        (column_name, &self.columns[*column_index])
    }

    // Returns a reference to the timestamp column.
    fn time_column(&self) -> &Column {
        &self.columns[self.time_column]
    }

    /// The time range of the `RowGroup` (of the time column).
    pub fn time_range(&self) -> (i64, i64) {
        self.meta.time_range
    }

    /// Efficiently determine if the provided predicate might be satisfied by
    /// the provided column.
    pub fn column_could_satisfy_predicate(
        &self,
        column_name: ColumnName<'_>,
        predicate: &(Operator, Value<'_>),
    ) -> bool {
        self.meta
            .read_group_could_satisfy_predicate(column_name, predicate)
    }

    //
    // Methods for reading the `RowGroup`
    //

    /// Returns a set of materialised column values that satisfy a set of
    /// predicates.
    ///
    /// Right now, predicates are conjunctive (AND).
    pub fn read_filter(
        &self,
        columns: &[ColumnName<'_>],
        predicates: &[Predicate<'_>],
    ) -> ReadFilterResult<'_> {
        let row_ids = self.row_ids_from_predicates(predicates);
        ReadFilterResult(self.materialise_rows(columns, row_ids))
    }

    fn materialise_rows(
        &self,
        names: &[ColumnName<'_>],
        row_ids: RowIDsOption,
    ) -> Vec<(ColumnName<'_>, Values<'_>)> {
        let mut results = vec![];
        match row_ids {
            RowIDsOption::None(_) => results, // nothing to materialise
            RowIDsOption::Some(row_ids) => {
                // TODO(edd): causes an allocation. Implement a way to pass a
                // pooled buffer to the croaring Bitmap API.
                let row_ids = row_ids.to_vec();
                for &name in names {
                    let (col_name, col) = self.column_name_and_column(name);
                    results.push((col_name, col.values(row_ids.as_slice())));
                }
                results
            }

            RowIDsOption::All(_) => {
                // TODO(edd): Perf - add specialised method to get all
                // materialised values from a column without having to
                // materialise a vector of row ids.......
                let row_ids = (0..self.rows()).collect::<Vec<_>>();

                for &name in names {
                    let (col_name, col) = self.column_name_and_column(name);
                    results.push((col_name, col.values(row_ids.as_slice())));
                }
                results
            }
        }
    }

    // Determines the set of row ids that satisfy the provided predicates. If
    // `predicates` contains two predicates on the time column they are
    // special-cased.
    fn row_ids_from_predicates(&self, predicates: &[Predicate<'_>]) -> RowIDsOption {
        // TODO(edd): perf - potentially pool this so we can re-use it once rows
        // have been materialised and it's no longer needed. Initialise a bitmap
        // RowIDs because it's like that set operations will be necessary.
        let mut result_row_ids = RowIDs::new_bitmap();

        // TODO(edd): perf - pool the dst buffer so we can re-use it across
        // subsequent calls to `row_ids_from_predicates`. Right now this buffer
        // will be re-used across all columns in the `RowGroup` but not re-used
        // for subsequent calls _to_ the `RowGroup`.
        let mut dst = RowIDs::new_bitmap();

        let mut predicates = Cow::Borrowed(predicates);
        // If there is a time-range in the predicates (two time predicates),
        // then execute an optimised version that will use a range based
        // predicate on the time column.
        if predicates
            .iter()
            .filter(|(col, _)| *col == TIME_COLUMN_NAME)
            .count()
            // Check if we have two predicates on the time column, i.e., a time
            // range.
            == 2
        {
            // Apply optimised filtering to time column
            let time_pred_row_ids =
                self.row_ids_from_predicates_with_time_range(predicates.as_ref(), dst);
            match time_pred_row_ids {
                // No matching rows based on time range
                RowIDsOption::None(_) => return time_pred_row_ids,

                // all rows match - continue to apply other predicates
                RowIDsOption::All(_dst) => {
                    dst = _dst; // hand buffer back
                }

                // some rows match - continue to apply predicates
                RowIDsOption::Some(row_ids) => {
                    // fill the result row id set with the matching rows from
                    // the time column.
                    result_row_ids.union(&row_ids);
                    dst = row_ids // hand buffer back
                }
            }

            // remove time predicates so they're not processed again
            let mut filtered_predicates = predicates.to_vec();
            filtered_predicates.retain(|(col, _)| *col != TIME_COLUMN_NAME);
            predicates = Cow::Owned(filtered_predicates);
        }

        for (name, (op, value)) in predicates.iter() {
            // N.B column should always exist because validation of predicates
            // should happen at the `Table` level.
            let (col_name, col) = self.column_name_and_column(name);

            // Explanation of how this buffer pattern works. The idea is that
            // the buffer should be returned to the caller so it can be re-used
            // on other columns. Each call to `row_ids_filter` returns the
            // buffer back enabling it to be re-used.
            match col.row_ids_filter(op, value, dst) {
                // No rows will be returned for the `RowGroup` because this
                // column does not match any rows.
                RowIDsOption::None(_dst) => return RowIDsOption::None(_dst),

                // Intersect the row ids found at this column with all those
                // found on other column predicates.
                RowIDsOption::Some(row_ids) => {
                    if result_row_ids.is_empty() {
                        result_row_ids.union(&row_ids)
                    }
                    result_row_ids.intersect(&row_ids);
                    dst = row_ids; // hand buffer back
                }

                // This is basically a no-op because all rows match the
                // predicate on this column.
                RowIDsOption::All(_dst) => {
                    dst = _dst; // hand buffer back
                }
            }
        }

        if result_row_ids.is_empty() {
            // All rows matched all predicates because any predicates not
            // matching any rows would have resulted in an early return.
            return RowIDsOption::All(result_row_ids);
        }
        RowIDsOption::Some(result_row_ids)
    }

    // An optimised function for applying two comparison predicates to a time
    // column at once.
    fn row_ids_from_predicates_with_time_range(
        &self,
        predicates: &[Predicate<'_>],
        dst: RowIDs,
    ) -> RowIDsOption {
        // find the time range predicates and execute a specialised range based
        // row id lookup.
        let time_predicates = predicates
            .iter()
            .filter(|(col_name, _)| col_name == &TIME_COLUMN_NAME)
            .collect::<Vec<_>>();
        assert!(time_predicates.len() == 2);

        self.time_column().row_ids_filter_range(
            &time_predicates[0].1, // min time
            &time_predicates[1].1, // max time
            dst,
        )
    }

    /// Returns a set of group keys and aggregated column data associated with
    /// them. `read_group` currently only supports grouping on columns that have
    /// integer encoded representations - typically "tag columns".
    ///
    /// Right now, predicates are treated conjunctive (AND) predicates.
    /// `read_group` does not guarantee any sort order. Ordering of results
    /// should be handled high up in the `Table` section of the Read Buffer,
    /// where multiple `RowGroup` results may need to be merged.
    pub fn read_group(
        &self,
        predicates: &[Predicate<'_>],
        group_columns: &[ColumnName<'_>],
        aggregates: &[(ColumnName<'_>, AggregateType)],
    ) -> ReadGroupResult<'_> {
        // `ReadGroupResult`s should have the same lifetime as self.
        // Alternatively ReadGroupResult could not store references to input
        // data and put the responsibility on the caller to tie result data and
        // input data together, but the convenience seems useful for now.
        let mut result = ReadGroupResult {
            group_columns: group_columns
                .iter()
                .map(|name| {
                    let (column_name, col) = self.column_name_and_column(name);
                    column_name
                })
                .collect::<Vec<_>>(),
            aggregate_columns: aggregates
                .iter()
                .map(|(name, typ)| {
                    let (column_name, col) = self.column_name_and_column(name);
                    (column_name, *typ)
                })
                .collect::<Vec<_>>(),
            ..ReadGroupResult::default()
        };

        // Handle case where there are no predicates and all the columns being
        // grouped support constant-time expression of the row_ids belonging to
        // each grouped value.
        let all_group_cols_pre_computed = result.group_columns.iter().all(|name| {
            self.column_by_name(name)
                .properties()
                .has_pre_computed_row_ids
        });
        if predicates.is_empty() && all_group_cols_pre_computed {
            self.read_group_all_rows_all_rle(&mut result);
            return result;
        }

        // There are predicates. The next stage is apply them and determine the
        // intermediate set of row ids.
        let row_ids = self.row_ids_from_predicates(predicates);
        let filter_row_ids = match row_ids {
            RowIDsOption::None(_) => {
                return result;
            } // no matching rows
            RowIDsOption::Some(row_ids) => Some(row_ids.to_vec()),
            RowIDsOption::All(row_ids) => None,
        };

        let group_cols_num = result.group_columns.len();
        let agg_cols_num = result.aggregate_columns.len();

        // materialise all *encoded* values for each column we are grouping on.
        // These will not be the logical (typically string) values, but will be
        // vectors of integers representing the physical values.
        let groupby_encoded_ids: Vec<_> = result
            .group_columns
            .iter()
            .map(|name| {
                let col = self.column_by_name(name);
                let mut encoded_values_buf =
                    EncodedValues::with_capacity_u32(col.num_rows() as usize);

                // Do we want some rows for the column (predicate filtered some
                // rows) or all of them (predicates filtered no rows).
                match &filter_row_ids {
                    Some(row_ids) => {
                        encoded_values_buf = col.encoded_values(row_ids, encoded_values_buf);
                    }
                    None => {
                        // None here means "no partial set of row ids" meaning
                        // get all of them.
                        encoded_values_buf = col.all_encoded_values(encoded_values_buf);
                    }
                }
                encoded_values_buf.take_u32()
            })
            .collect();

        // Materialise values in aggregate columns.
        let mut aggregate_columns_data = Vec::with_capacity(agg_cols_num);
        for (name, agg_type) in &result.aggregate_columns {
            let col = self.column_by_name(name);

            // TODO(edd): this materialises a column per aggregate. If there are
            // multiple aggregates for the same column then this will
            // over-allocate

            // Do we want some rows for the column or all of them?
            let column_values = match &filter_row_ids {
                Some(row_ids) => col.values(row_ids),
                None => {
                    // None here means "no partial set of row ids", i.e., get
                    // all of the row ids because they all satisfy the
                    // predicates.
                    col.all_values()
                }
            };
            aggregate_columns_data.push(column_values);
        }

        // If there is a single group column then we can use an optimised
        // approach for building group keys
        if group_columns.len() == 1 {
            self.read_group_single_group_column(
                &mut result,
                &groupby_encoded_ids[0],
                aggregate_columns_data,
            );
            return result;
        }

        // Perform the group by using a hashmap
        self.read_group_with_hashing(&mut result, &groupby_encoded_ids, aggregate_columns_data);
        result
    }

    // read_group_hash executes a read-group-aggregate operation on the
    // `RowGroup` using a hashmap to build up a collection of group keys and
    // aggregates.
    //
    // read_group_hash accepts a set of conjunctive predicates.
    fn read_group_with_hashing<'a>(
        &'a self,
        dst: &mut ReadGroupResult<'a>,
        groupby_encoded_ids: &[Vec<u32>],
        aggregate_columns_data: Vec<Values<'a>>,
    ) {
        // An optimised approach to building the hashmap of group keys using a
        // single 128-bit integer as the group key. If grouping is on more than
        // four columns then a fallback to using an vector as a key will happen.
        if dst.group_columns.len() <= 4 {
            self.read_group_hash_with_u128_key(dst, &groupby_encoded_ids, &aggregate_columns_data);
            return;
        }

        self.read_group_hash_with_vec_key(dst, &groupby_encoded_ids, &aggregate_columns_data);
    }

    // This function is used with `read_group_hash` when the number of columns
    // being grouped on requires the use of a `Vec<u32>` as the group key in the
    // hash map.
    fn read_group_hash_with_vec_key<'a>(
        &'a self,
        dst: &mut ReadGroupResult<'a>,
        groupby_encoded_ids: &[Vec<u32>],
        aggregate_columns_data: &[Values<'a>],
    ) {
        // Now begin building the group keys.
        let mut groups: HashMap<Vec<u32>, Vec<AggregateResult<'_>>> = HashMap::default();
        let total_rows = groupby_encoded_ids[0].len();
        assert!(groupby_encoded_ids.iter().all(|x| x.len() == total_rows));

        // key_buf will be used as a temporary buffer for group keys, which are
        // themselves integers.
        let mut key_buf = vec![0; dst.group_columns.len()];

        for row in 0..total_rows {
            // update the group key buffer with the group key for this row
            for (j, col_ids) in groupby_encoded_ids.iter().enumerate() {
                key_buf[j] = col_ids[row];
            }

            match groups.raw_entry_mut().from_key(&key_buf) {
                // aggregates for this group key are already present. Update
                // them
                hash_map::RawEntryMut::Occupied(mut entry) => {
                    for (i, values) in aggregate_columns_data.iter().enumerate() {
                        entry.get_mut()[i].update(values.value(row));
                    }
                }
                // group key does not exist, so create it.
                hash_map::RawEntryMut::Vacant(entry) => {
                    let mut group_key_aggs = Vec::with_capacity(dst.aggregate_columns.len());
                    for (_, agg_type) in &dst.aggregate_columns {
                        group_key_aggs.push(AggregateResult::from(agg_type));
                    }

                    for (i, values) in aggregate_columns_data.iter().enumerate() {
                        group_key_aggs[i].update(values.value(row));
                    }

                    entry.insert(key_buf.clone(), group_key_aggs);
                }
            }
        }

        // Finally, build results set. Each encoded group key needs to be
        // materialised into a logical group key
        let columns = dst
            .group_columns
            .iter()
            .map(|name| self.column_by_name(name))
            .collect::<Vec<_>>();
        let mut group_key_vec: Vec<GroupKey<'_>> = Vec::with_capacity(groups.len());
        let mut aggregate_vec = Vec::with_capacity(groups.len());

        for (group_key, aggs) in groups.into_iter() {
            let mut logical_key = Vec::with_capacity(group_key.len());
            for (col_idx, &encoded_id) in group_key.iter().enumerate() {
                // TODO(edd): address the cast to u32
                logical_key.push(columns[col_idx].decode_id(encoded_id as u32));
            }

            group_key_vec.push(GroupKey(logical_key));
            aggregate_vec.push(aggs.clone());
        }

        // update results
        dst.group_keys = group_key_vec;
        dst.aggregates = aggregate_vec;
    }

    // This function is similar to `read_group_hash_with_vec_key` in that it
    // calculates groups keys and aggregates for a read-group-aggregate
    // operation using a hashmap.
    //
    // This function can be invoked when fewer than four columns are being
    // grouped. In this case the key to the hashmap can be a `u128` integer,
    // which is significantly more performant than using a `Vec<u32>`.
    fn read_group_hash_with_u128_key<'a>(
        &'a self,
        dst: &mut ReadGroupResult<'a>,
        groupby_encoded_ids: &[Vec<u32>],
        aggregate_columns_data: &[Values<'a>],
    ) {
        let total_rows = groupby_encoded_ids[0].len();
        assert!(groupby_encoded_ids.iter().all(|x| x.len() == total_rows));
        assert!(dst.group_columns.len() <= 4);

        // Now begin building the group keys.
        let mut groups: HashMap<u128, Vec<AggregateResult<'_>>> = HashMap::default();

        for row in 0..groupby_encoded_ids[0].len() {
            // pack each column's encoded value for the row into a packed group
            // key.
            let mut group_key_packed = 0_u128;
            for (i, col_ids) in groupby_encoded_ids.iter().enumerate() {
                group_key_packed = pack_u32_in_u128(group_key_packed, col_ids[row], i);
            }

            match groups.raw_entry_mut().from_key(&group_key_packed) {
                // aggregates for this group key are already present. Update
                // them
                hash_map::RawEntryMut::Occupied(mut entry) => {
                    for (i, values) in aggregate_columns_data.iter().enumerate() {
                        entry.get_mut()[i].update(values.value(row));
                    }
                }
                // group key does not exist, so create it.
                hash_map::RawEntryMut::Vacant(entry) => {
                    let mut group_key_aggs = Vec::with_capacity(dst.aggregate_columns.len());
                    for (_, agg_type) in &dst.aggregate_columns {
                        group_key_aggs.push(AggregateResult::from(agg_type));
                    }

                    for (i, values) in aggregate_columns_data.iter().enumerate() {
                        group_key_aggs[i].update(values.value(row));
                    }

                    entry.insert(group_key_packed, group_key_aggs);
                }
            }
        }

        // Finally, build results set. Each encoded group key needs to be
        // materialised into a logical group key
        let columns = dst
            .group_columns
            .iter()
            .map(|name| self.column_by_name(name))
            .collect::<Vec<_>>();
        let mut group_key_vec: Vec<GroupKey<'_>> = Vec::with_capacity(groups.len());
        let mut aggregate_vec = Vec::with_capacity(groups.len());

        for (group_key_packed, aggs) in groups.into_iter() {
            let mut logical_key = Vec::with_capacity(columns.len());

            // Unpack the appropriate encoded id for each column from the packed
            // group key, then materialise the logical value for that id and add
            // it to the materialised group key (`logical_key`).
            for (col_idx, column) in columns.iter().enumerate() {
                let encoded_id = (group_key_packed >> (col_idx * 32)) as u32;
                logical_key.push(column.decode_id(encoded_id));
            }

            group_key_vec.push(GroupKey(logical_key));
            aggregate_vec.push(aggs.clone());
        }

        dst.group_keys = group_key_vec;
        dst.aggregates = aggregate_vec;
    }

    // Optimised `read_group` method when there are no predicates and all the
    // group columns are RLE-encoded.
    //
    // In this case all the grouping columns pre-computed bitsets for each
    // distinct value.
    fn read_group_all_rows_all_rle<'a>(&'a self, dst: &mut ReadGroupResult<'a>) {
        let group_columns = dst
            .group_columns
            .iter()
            .map(|name| self.column_by_name(name))
            .collect::<Vec<_>>();

        let aggregate_columns_typ = dst
            .aggregate_columns
            .iter()
            .map(|(name, typ)| (self.column_by_name(name), *typ))
            .collect::<Vec<_>>();

        let encoded_groups = dst
            .group_columns
            .iter()
            .map(|name| self.column_by_name(name).grouped_row_ids().unwrap_left())
            .collect::<Vec<_>>();

        // multi_cartesian_product will create the cartesian product of all
        // grouping-column values. This is likely going to be more group keys
        // than there exists row-data for, so don't materialise them yet...
        //
        // For example, we have two columns like:
        //
        //    [0, 1, 1, 2, 2, 3, 4] // column encodes the values as integers [3,
        //    3, 3, 3, 4, 2, 1] // column encodes the values as integers
        //
        // The columns have these distinct values:
        //
        //    [0, 1, 2, 3, 4] [1, 2, 3, 4]
        //
        // We will produce the following "group key" candidates:
        //
        //    [0, 1], [0, 2], [0, 3], [0, 4] [1, 1], [1, 2], [1, 3], [1, 4] [2,
        //    1], [2, 2], [2, 3], [2, 4] [3, 1], [3, 2], [3, 3], [3, 4] [4, 1],
        //    [4, 2], [4, 3], [4, 4]
        //
        // Based on the columns we can see that we only have data for the
        // following group keys:
        //
        //    [0, 3], [1, 3], [2, 3], [2, 4], [3, 2], [4, 1]
        //
        // We figure out which group keys have data and which don't in the loop
        // below, by intersecting bitsets for each id and checking for non-empty
        // sets.
        let group_keys = encoded_groups
            .iter()
            .map(|ids| (0..ids.len()))
            .multi_cartesian_product();

        // Let's figure out which of the candidate group keys are actually group
        // keys with data.
        'outer: for group_key in group_keys {
            let mut aggregate_row_ids =
                Cow::Borrowed(encoded_groups[0][group_key[0]].unwrap_bitmap());

            if aggregate_row_ids.is_empty() {
                continue;
            }

            for i in 1..group_key.len() {
                let other = encoded_groups[i][group_key[i]].unwrap_bitmap();

                if aggregate_row_ids.and_cardinality(other) > 0 {
                    aggregate_row_ids = Cow::Owned(aggregate_row_ids.and(other));
                } else {
                    continue 'outer;
                }
            }

            // This group key has some matching row ids. Materialise the group
            // key and calculate the aggregates.

            // TODO(edd): given these RLE columns should have low cardinality
            // there should be a reasonably low group key cardinality. It could
            // be safe to use `small_vec` here without blowing the stack up.
            let mut material_key = Vec::with_capacity(group_key.len());
            for (col_idx, &encoded_id) in group_key.iter().enumerate() {
                material_key.push(group_columns[col_idx].decode_id(encoded_id as u32));
            }
            dst.group_keys.push(GroupKey(material_key));

            let mut aggregates = Vec::with_capacity(aggregate_columns_typ.len());
            for (agg_col, typ) in &aggregate_columns_typ {
                aggregates.push(match typ {
                    AggregateType::Count => {
                        AggregateResult::Count(agg_col.count(&aggregate_row_ids.to_vec()) as u64)
                    }
                    AggregateType::First => todo!(),
                    AggregateType::Last => todo!(),
                    AggregateType::Min => {
                        AggregateResult::Min(agg_col.min(&aggregate_row_ids.to_vec()))
                    }
                    AggregateType::Max => {
                        AggregateResult::Max(agg_col.max(&aggregate_row_ids.to_vec()))
                    }
                    AggregateType::Sum => {
                        AggregateResult::Sum(agg_col.sum(&aggregate_row_ids.to_vec()))
                    }
                });
            }
            dst.aggregates.push(aggregates);
        }
    }

    // Optimised `read_group` path for queries where only a single column is
    // being grouped on. In this case building a hash table is not necessary,
    // and the group keys can be used as indexes into a vector whose values
    // contain aggregates. As rows are processed these aggregates can be updated
    // in constant time.
    fn read_group_single_group_column<'a>(
        &'a self,
        dst: &mut ReadGroupResult<'a>,
        groupby_encoded_ids: &[u32],
        aggregate_columns_data: Vec<Values<'a>>,
    ) {
        let column = self.column_by_name(dst.group_columns[0]);
        assert_eq!(dst.group_columns.len(), aggregate_columns_data.len());
        let total_rows = groupby_encoded_ids.len();

        // Allocate a vector to hold aggregates that can be updated as rows are
        // processed. An extra group is required because encoded ids are
        // 0-indexed.
        let required_groups = groupby_encoded_ids.iter().max().unwrap() + 1;
        let mut groups: Vec<Option<Vec<AggregateResult<'_>>>> =
            vec![None; required_groups as usize];

        for (row, encoded_id) in groupby_encoded_ids.iter().enumerate() {
            let idx = *encoded_id as usize;
            match &mut groups[idx] {
                Some(group_key_aggs) => {
                    // Update all aggregates for the group key
                    for (i, values) in aggregate_columns_data.iter().enumerate() {
                        group_key_aggs[i].update(values.value(row));
                    }
                }
                None => {
                    let mut group_key_aggs = dst
                        .aggregate_columns
                        .iter()
                        .map(|(_, agg_type)| AggregateResult::from(agg_type))
                        .collect::<Vec<_>>();

                    for (i, values) in aggregate_columns_data.iter().enumerate() {
                        group_key_aggs[i].update(values.value(row));
                    }

                    groups[idx] = Some(group_key_aggs);
                }
            }
        }

        // Finally, build results set. Each encoded group key needs to be
        // materialised into a logical group key
        let mut group_key_vec: Vec<GroupKey<'_>> = Vec::with_capacity(groups.len());
        let mut aggregate_vec = Vec::with_capacity(groups.len());

        for (group_key, aggs) in groups.into_iter().enumerate() {
            if let Some(aggs) = aggs {
                group_key_vec.push(GroupKey(vec![column.decode_id(group_key as u32)]));
                aggregate_vec.push(aggs);
            }
        }

        dst.group_keys = group_key_vec;
        dst.aggregates = aggregate_vec;
    }

    // Optimised `read_group` method for cases where the columns being grouped
    // are already totally ordered in the `RowGroup`.
    //
    // In this case the rows are already in "group key order" and the aggregates
    // can be calculated by reading the rows in order.
    fn read_group_sorted_stream(
        &self,
        predicates: &[Predicate<'_>],
        group_column: ColumnName<'_>,
        aggregates: &[(ColumnName<'_>, AggregateType)],
    ) {
        todo!()
    }
}

// Packs an encoded values into a `u128` at `pos`, which must be `[0,4)`.
#[inline(always)]
fn pack_u32_in_u128(packed_value: u128, encoded_id: u32, pos: usize) -> u128 {
    packed_value | (encoded_id as u128) << (32 * pos)
}

// Given a packed encoded group key, unpacks them into `n` individual `u32`
// group keys, and stores them in `dst`. It is the caller's responsibility to
// ensure n <= 4.
fn unpack_u128_group_key(group_key_packed: u128, n: usize, mut dst: Vec<u32>) -> Vec<u32> {
    dst.resize(n, 0);

    for (i, encoded_id) in dst.iter_mut().enumerate() {
        *encoded_id = (group_key_packed >> (i * 32)) as u32;
    }

    dst
}

pub type Predicate<'a> = (ColumnName<'a>, (Operator, Value<'a>));

// A GroupKey is an ordered collection of row values. The order determines which
// columns the values originated from.
#[derive(PartialEq, PartialOrd, Clone)]
pub struct GroupKey<'row_group>(Vec<Value<'row_group>>);

impl Eq for GroupKey<'_> {}

// Implementing the `Ord` trait on `GroupKey` means that collections of group
// keys become sortable. This is typically useful for test because depending on
// the implementation, group keys are not always emitted in sorted order.
//
// To be compared, group keys *must* have the same length, or `cmp` will panic.
// They will be ordered as follows:
//
//    [foo, zoo, zoo], [foo, bar, zoo], [bar, bar, bar], [bar, bar, zoo],
//
//    becomes:
//
//    [bar, bar, bar], [bar, bar, zoo], [foo, bar, zoo], [foo, zoo, zoo],
//
// Be careful sorting group keys in result sets, because other columns
// associated with the group keys won't be sorted unless the correct `sort`
// methods are used on the result set implementations.
impl Ord for GroupKey<'_> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // two group keys must have same length
        assert_eq!(self.0.len(), other.0.len());

        let cols = self.0.len();
        for i in 0..cols {
            match self.0[i].partial_cmp(&other.0[i]) {
                Some(ord) => return ord,
                None => continue,
            }
        }

        std::cmp::Ordering::Equal
    }
}

// A representation of a column name.
pub type ColumnName<'a> = &'a str;

/// The logical type that a column could have.
pub enum ColumnType {
    Tag(Column),
    Field(Column),
    Time(Column),
}

impl ColumnType {
    // The total size in bytes of the column
    pub fn size(&self) -> u64 {
        match &self {
            ColumnType::Tag(c) => c.size(),
            ColumnType::Field(c) => c.size(),
            ColumnType::Time(c) => c.size(),
        }
    }
}

#[derive(Default, Debug)]
struct MetaData {
    // The total size of the table in bytes.
    size: u64,

    // The total number of rows in the table.
    rows: u32,

    // The distinct set of columns for this `RowGroup` (all of these columns
    // will appear in all of the `Table`'s `RowGroup`s) and the range of values
    // for each of those columns.
    //
    // This can be used to skip the table entirely if a logical predicate can't
    // possibly match based on the range of values a column has.
    column_ranges: BTreeMap<String, (OwnedValue, OwnedValue)>,

    // The total time range of this table spanning all of the `RowGroup`s within
    // the table.
    //
    // This can be used to skip the table entirely if the time range for a query
    // falls outside of this range.
    time_range: (i64, i64),
}

impl MetaData {
    // helper function to determine if the provided predicate could be satisfied
    // by the `RowGroup`. If this function returns `false` then there is no
    // point attempting to read data from the `RowGroup`.
    //
    pub fn read_group_could_satisfy_predicate(
        &self,
        column_name: ColumnName<'_>,
        predicate: &(Operator, Value<'_>),
    ) -> bool {
        let (column_min, column_max) = match self.column_ranges.get(column_name) {
            Some(range) => range,
            None => return false, // column doesn't exist.
        };

        let (op, value) = predicate;
        match op {
            // If the column range covers the value then it could contain that
            // value.
            Operator::Equal => column_min <= value && column_max >= value,

            // If every value in the column is equal to "value" then this will
            // be false, otherwise it must be satisfied
            Operator::NotEqual => (column_min != column_max) || column_max != value,

            // if the column max is larger than value then the column could
            // contain the value.
            Operator::GT => column_max > value,

            // if the column max is at least as large as `value` then the column
            // could contain the value.
            Operator::GTE => column_max >= value,

            // if the column min is smaller than value then the column could
            // contain the value.
            Operator::LT => column_min < value,

            // if the column min is at least as small as value then the column
            // could contain the value.
            Operator::LTE => column_min <= value,
        }
    }
}

/// Encapsulates results from `RowGroup`s with a structure that makes them
/// easier to work with and display.
pub struct ReadFilterResult<'row_group>(pub Vec<(ColumnName<'row_group>, Values<'row_group>)>);

impl ReadFilterResult<'_> {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::fmt::Debug for &ReadFilterResult<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // header line.
        for (i, (k, _)) in self.0.iter().enumerate() {
            write!(f, "{}", k)?;

            if i < self.0.len() - 1 {
                write!(f, ",")?;
            }
        }
        writeln!(f)?;

        // Display the rest of the values.
        std::fmt::Display::fmt(&self, f)
    }
}

impl std::fmt::Display for &ReadFilterResult<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.is_empty() {
            return Ok(());
        }

        let expected_rows = self.0[0].1.len();
        let mut rows = 0;

        let mut iter_map = self
            .0
            .iter()
            .map(|(k, v)| (*k, ValuesIterator::new(v)))
            .collect::<BTreeMap<&str, ValuesIterator<'_>>>();

        while rows < expected_rows {
            if rows > 0 {
                writeln!(f)?;
            }

            for (i, (k, _)) in self.0.iter().enumerate() {
                if let Some(itr) = iter_map.get_mut(k) {
                    write!(f, "{}", itr.next().unwrap())?;
                    if i < self.0.len() - 1 {
                        write!(f, ",")?;
                    }
                }
            }

            rows += 1;
        }
        writeln!(f)
    }
}

#[derive(Default)]
pub struct ReadGroupResult<'row_group> {
    // columns that are being grouped on.
    group_columns: Vec<ColumnName<'row_group>>,

    // columns that are being aggregated
    aggregate_columns: Vec<(ColumnName<'row_group>, AggregateType)>,

    // row-wise collection of group keys. Each group key contains column-wise
    // values for each of the groupby_columns.
    group_keys: Vec<GroupKey<'row_group>>,

    // row-wise collection of aggregates. Each aggregate contains column-wise
    // values for each of the aggregate_columns.
    aggregates: Vec<Vec<AggregateResult<'row_group>>>,
}

impl ReadGroupResult<'_> {
    pub fn is_empty(&self) -> bool {
        self.group_keys.is_empty()
    }

    // The number of distinct group keys in the result.
    pub fn cardinality(&self) -> usize {
        self.group_keys.len()
    }

    /// Executes a mutable sort of the rows in the result set based on the
    /// lexicographic order of each group key column. This is useful for testing
    /// because it allows you to compare `read_group` results.
    pub fn sort(&mut self) {
        // The permutation crate lets you execute a sort on anything implements
        // `Ord` and return the sort order, which can then be applied to other
        // columns.
        let perm = permutation::sort(self.group_keys.as_slice());
        self.group_keys = perm.apply_slice(self.group_keys.as_slice());
        self.aggregates = perm.apply_slice(self.aggregates.as_slice());
    }
}

impl std::fmt::Debug for &ReadGroupResult<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // group column names
        for k in &self.group_columns {
            write!(f, "{},", k)?;
        }

        // aggregate column names
        for (i, (k, typ)) in self.aggregate_columns.iter().enumerate() {
            write!(f, "{}_{}", k, typ)?;

            if i < self.aggregate_columns.len() - 1 {
                write!(f, ",")?;
            }
        }
        writeln!(f)?;

        // Display the rest of the values.
        std::fmt::Display::fmt(&self, f)
    }
}

impl std::fmt::Display for &ReadGroupResult<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.is_empty() {
            return Ok(());
        }

        let expected_rows = self.group_keys.len();
        for row in 0..expected_rows {
            if row > 0 {
                writeln!(f)?;
            }

            // write row for group by columns
            for value in self.group_keys[row].0.iter() {
                write!(f, "{},", value)?;
            }

            // write row for aggregate columns
            for (col_i, agg) in self.aggregates[row].iter().enumerate() {
                write!(f, "{}", agg)?;
                if col_i < self.aggregates[row].len() - 1 {
                    write!(f, ",")?;
                }
            }
        }

        writeln!(f)
    }
}

/// helper function useful for tests and benchmarks. Creates a time-range
/// predicate in the domain `[from, to)`.
pub fn build_predicates_with_time(
    from: i64,
    to: i64,
    others: Vec<Predicate<'_>>,
) -> Vec<Predicate<'_>> {
    let mut arr = vec![
        (
            TIME_COLUMN_NAME,
            (Operator::GTE, Value::Scalar(Scalar::I64(from))),
        ),
        (
            TIME_COLUMN_NAME,
            (Operator::LT, Value::Scalar(Scalar::I64(to))),
        ),
    ];

    arr.extend(others);
    arr
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn row_ids_from_predicates() {
        let mut columns = BTreeMap::new();
        let tc = ColumnType::Time(Column::from(&[100_i64, 200, 500, 600, 300, 300][..]));
        columns.insert("time".to_string(), tc);
        let rc = ColumnType::Tag(Column::from(
            &["west", "west", "east", "west", "south", "north"][..],
        ));
        columns.insert("region".to_string(), rc);
        let row_group = RowGroup::new(6, columns);

        // Closed partially covering "time range" predicate
        let row_ids =
            row_group.row_ids_from_predicates(&build_predicates_with_time(200, 600, vec![]));
        assert_eq!(row_ids.unwrap().to_vec(), vec![1, 2, 4, 5]);

        // Fully covering "time range" predicate
        let row_ids =
            row_group.row_ids_from_predicates(&build_predicates_with_time(10, 601, vec![]));
        assert!(matches!(row_ids, RowIDsOption::All(_)));

        // Open ended "time range" predicate
        let row_ids = row_group.row_ids_from_predicates(&[(
            TIME_COLUMN_NAME,
            (Operator::GTE, Value::Scalar(Scalar::I64(300))),
        )]);
        assert_eq!(row_ids.unwrap().to_vec(), vec![2, 3, 4, 5]);

        // Closed partially covering "time range" predicate and other column
        // predicate
        let row_ids = row_group.row_ids_from_predicates(&build_predicates_with_time(
            200,
            600,
            vec![("region", (Operator::Equal, Value::String("south")))],
        ));
        assert_eq!(row_ids.unwrap().to_vec(), vec![4]);

        // Fully covering "time range" predicate and other column predicate
        let row_ids = row_group.row_ids_from_predicates(&build_predicates_with_time(
            10,
            601,
            vec![("region", (Operator::Equal, Value::String("west")))],
        ));
        assert_eq!(row_ids.unwrap().to_vec(), vec![0, 1, 3]);

        // "time range" predicate and other column predicate that doesn't match
        let row_ids = row_group.row_ids_from_predicates(&build_predicates_with_time(
            200,
            600,
            vec![("region", (Operator::Equal, Value::String("nope")))],
        ));
        assert!(matches!(row_ids, RowIDsOption::None(_)));

        // Just a column predicate
        let row_ids = row_group
            .row_ids_from_predicates(&[("region", (Operator::Equal, Value::String("east")))]);
        assert_eq!(row_ids.unwrap().to_vec(), vec![2]);

        // Predicate can matches all the rows
        let row_ids = row_group
            .row_ids_from_predicates(&[("region", (Operator::NotEqual, Value::String("abba")))]);
        assert!(matches!(row_ids, RowIDsOption::All(_)));

        // No predicates
        let row_ids = row_group.row_ids_from_predicates(&[]);
        assert!(matches!(row_ids, RowIDsOption::All(_)));
    }

    #[test]
    fn read_filter() {
        let mut columns = BTreeMap::new();
        let tc = ColumnType::Time(Column::from(&[1_i64, 2, 3, 4, 5, 6][..]));
        columns.insert("time".to_string(), tc);

        let rc = ColumnType::Tag(Column::from(
            &["west", "west", "east", "west", "south", "north"][..],
        ));
        columns.insert("region".to_string(), rc);

        let mc = ColumnType::Tag(Column::from(
            &["GET", "POST", "POST", "POST", "PUT", "GET"][..],
        ));
        columns.insert("method".to_string(), mc);

        let fc = ColumnType::Field(Column::from(&[100_u64, 101, 200, 203, 203, 10][..]));
        columns.insert("count".to_string(), fc);

        let row_group = RowGroup::new(6, columns);

        let cases = vec![
            (
                vec!["count", "region", "time"],
                build_predicates_with_time(1, 6, vec![]),
                "count,region,time
100,west,1
101,west,2
200,east,3
203,west,4
203,south,5
",
            ),
            (
                vec!["time", "region", "method"],
                build_predicates_with_time(-19, 2, vec![]),
                "time,region,method
1,west,GET
",
            ),
            (
                vec!["time"],
                build_predicates_with_time(0, 3, vec![]),
                "time
1
2
",
            ),
            (
                vec!["method"],
                build_predicates_with_time(0, 3, vec![]),
                "method
GET
POST
",
            ),
            (
                vec!["count", "method", "time"],
                build_predicates_with_time(
                    0,
                    6,
                    vec![("method", (Operator::Equal, Value::String("POST")))],
                ),
                "count,method,time
101,POST,2
200,POST,3
203,POST,4
",
            ),
            (
                vec!["region", "time"],
                build_predicates_with_time(
                    0,
                    6,
                    vec![("method", (Operator::Equal, Value::String("POST")))],
                ),
                "region,time
west,2
east,3
west,4
",
            ),
        ];

        for (cols, predicates, expected) in cases {
            let results = row_group.read_filter(&cols, &predicates);
            assert_eq!(format!("{:?}", &results), expected);
        }

        // test no matching rows
        let results = row_group.read_filter(
            &["method", "region", "time"],
            &build_predicates_with_time(-19, 1, vec![]),
        );
        let expected = "";
        assert!(results.is_empty());
    }

    #[test]
    fn read_group() {
        let mut columns = BTreeMap::new();
        let tc = ColumnType::Time(Column::from(&[1_i64, 2, 3, 4, 5, 6][..]));
        columns.insert("time".to_string(), tc);

        let rc = ColumnType::Tag(Column::from(
            &["west", "west", "east", "west", "south", "north"][..],
        ));
        columns.insert("region".to_string(), rc);

        let mc = ColumnType::Tag(Column::from(
            &["GET", "POST", "POST", "POST", "PUT", "GET"][..],
        ));
        columns.insert("method".to_string(), mc);

        let ec = ColumnType::Tag(Column::from(
            &[
                Some("prod"),
                Some("prod"),
                Some("stag"),
                Some("prod"),
                None,
                None,
            ][..],
        ));
        columns.insert("env".to_string(), ec);

        let c = ColumnType::Tag(Column::from(
            &["Alpha", "Alpha", "Bravo", "Bravo", "Alpha", "Alpha"][..],
        ));
        columns.insert("letters".to_string(), c);

        let c = ColumnType::Tag(Column::from(
            &["one", "two", "two", "two", "one", "three"][..],
        ));
        columns.insert("numbers".to_string(), c);

        let fc = ColumnType::Field(Column::from(&[100_u64, 101, 200, 203, 203, 10][..]));
        columns.insert("counter".to_string(), fc);

        let row_group = RowGroup::new(6, columns);

        // test queries with no predicates and grouping on low cardinality
        // columns
        read_group_all_rows_all_rle(&row_group);

        // test read group queries that group on fewer than five columns.
        read_group_hash_u128_key(&row_group);

        // test read group queries that use a vector-based group key.
        read_group_hash_vec_key(&row_group);

        // test read group queries that only group on one column.
        read_group_single_groupby_column(&row_group);
    }

    // the read_group path where grouping is on fewer than five columns.
    fn read_group_hash_u128_key(row_group: &RowGroup) {
        let cases = vec![
            (
                build_predicates_with_time(0, 7, vec![]), // all time but without explicit pred
                vec!["region", "method"],
                vec![("counter", AggregateType::Sum)],
                "region,method,counter_sum
east,POST,200
north,GET,10
south,PUT,203
west,GET,100
west,POST,304
",
            ),
            (
                build_predicates_with_time(2, 6, vec![]), // all time but without explicit pred
                vec!["env", "region"],
                vec![
                    ("counter", AggregateType::Sum),
                    ("counter", AggregateType::Count),
                ],
                "env,region,counter_sum,counter_count
NULL,south,203,1
prod,west,304,2
stag,east,200,1
",
            ),
            (
                build_predicates_with_time(-1, 10, vec![]),
                vec!["region", "env"],
                vec![("method", AggregateType::Min)], // Yep, you can aggregate any column.
                "region,env,method_min
east,stag,POST
north,NULL,GET
south,NULL,PUT
west,prod,GET
",
            ),
            // This case is identical to above but has an explicit `region >
            // "north"` predicate.
            (
                build_predicates_with_time(
                    -1,
                    10,
                    vec![("region", (Operator::GT, Value::String("north")))],
                ),
                vec!["region", "env"],
                vec![("method", AggregateType::Min)], // Yep, you can aggregate any column.
                "region,env,method_min
south,NULL,PUT
west,prod,GET
",
            ),
            (
                build_predicates_with_time(-1, 10, vec![]),
                vec!["region", "env", "method"],
                vec![("time", AggregateType::Max)], // Yep, you can aggregate any column.
                "region,env,method,time_max
east,stag,POST,3
north,NULL,GET,6
south,NULL,PUT,5
west,prod,GET,1
west,prod,POST,4
",
            ),
        ];

        for (predicate, group_cols, aggs, expected) in cases {
            let mut results = row_group.read_group(&predicate, &group_cols, &aggs);
            results.sort();
            assert_eq!(format!("{:?}", &results), expected);
        }
    }

    // the read_group path where grouping is on five or more columns. This will
    // ensure that the `read_group_hash_with_vec_key` path is exercised.
    fn read_group_hash_vec_key(row_group: &RowGroup) {
        let cases = vec![(
            build_predicates_with_time(0, 7, vec![]), // all time but with explicit pred
            vec!["region", "method", "env", "letters", "numbers"],
            vec![("counter", AggregateType::Sum)],
            "region,method,env,letters,numbers,counter_sum
east,POST,stag,Bravo,two,200
north,GET,NULL,Alpha,three,10
south,PUT,NULL,Alpha,one,203
west,GET,prod,Alpha,one,100
west,POST,prod,Alpha,two,101
west,POST,prod,Bravo,two,203
",
        )];

        for (predicate, group_cols, aggs, expected) in cases {
            let mut results = row_group.read_group(&predicate, &group_cols, &aggs);
            results.sort();
            assert_eq!(format!("{:?}", &results), expected);
        }
    }

    // the read_group path where grouping is on a single column.
    fn read_group_single_groupby_column(row_group: &RowGroup) {
        let cases = vec![(
            build_predicates_with_time(0, 7, vec![]), // all time but with explicit pred
            vec!["method"],
            vec![("counter", AggregateType::Sum)],
            "method,counter_sum
GET,110
POST,504
PUT,203
",
        )];

        for (predicate, group_cols, aggs, expected) in cases {
            let mut results = row_group.read_group(&predicate, &group_cols, &aggs);
            results.sort();
            assert_eq!(format!("{:?}", &results), expected);
        }
    }

    fn read_group_all_rows_all_rle(row_group: &RowGroup) {
        let cases = vec![
            (
                vec![],
                vec!["region", "method"],
                vec![("counter", AggregateType::Sum)],
                "region,method,counter_sum
east,POST,200
north,GET,10
south,PUT,203
west,GET,100
west,POST,304
",
            ),
            (
                vec![],
                vec!["region", "method", "env"],
                vec![("counter", AggregateType::Sum)],
                "region,method,env,counter_sum
east,POST,stag,200
north,GET,NULL,10
south,PUT,NULL,203
west,GET,prod,100
west,POST,prod,304
",
            ),
            (
                vec![],
                vec!["env"],
                vec![("counter", AggregateType::Count)],
                "env,counter_count
NULL,2
prod,3
stag,1
",
            ),
            (
                vec![],
                vec!["region", "method"],
                vec![
                    ("counter", AggregateType::Sum),
                    ("counter", AggregateType::Min),
                    ("counter", AggregateType::Max),
                ],
                "region,method,counter_sum,counter_min,counter_max
east,POST,200,200,200
north,GET,10,10,10
south,PUT,203,203,203
west,GET,100,100,100
west,POST,304,101,203
",
            ),
        ];

        for (predicate, group_cols, aggs, expected) in cases {
            let results = row_group.read_group(&predicate, &group_cols, &aggs);
            assert_eq!(format!("{:?}", &results), expected);
        }
    }

    #[test]
    fn row_group_could_satisfy_predicate() {
        let mut columns = BTreeMap::new();
        let tc = ColumnType::Time(Column::from(&[1_i64, 2, 3, 4, 5, 6][..]));
        columns.insert("time".to_string(), tc);

        let rc = ColumnType::Tag(Column::from(
            &["west", "west", "east", "west", "south", "north"][..],
        ));
        columns.insert("region".to_string(), rc);

        let mc = ColumnType::Tag(Column::from(
            &["GET", "GET", "GET", "GET", "GET", "GET"][..],
        ));
        columns.insert("method".to_string(), mc);

        let row_group = RowGroup::new(6, columns);

        let cases = vec![
            ("az", &(Operator::Equal, Value::String("west")), false), // no az column
            ("region", &(Operator::Equal, Value::String("west")), true), /* region column does
                                                                       * contain "west" */
            ("region", &(Operator::Equal, Value::String("over")), true), /* region column might
                                                                          * contain "over" */
            ("region", &(Operator::Equal, Value::String("abc")), false), /* region column can't
                                                                          * contain "abc" */
            ("region", &(Operator::Equal, Value::String("zoo")), false), /* region column can't
                                                                          * contain "zoo" */
            (
                "region",
                &(Operator::NotEqual, Value::String("hello")),
                true,
            ), // region column might not contain "hello"
            ("method", &(Operator::NotEqual, Value::String("GET")), false), /* method must only
                                                                             * contain "GET" */
            ("region", &(Operator::GT, Value::String("abc")), true), /* region column might
                                                                      * contain something >
                                                                      * "abc" */
            ("region", &(Operator::GT, Value::String("north")), true), /* region column might
                                                                        * contain something >
                                                                        * "north" */
            ("region", &(Operator::GT, Value::String("west")), false), /* region column can't
                                                                        * contain something >
                                                                        * "west" */
            ("region", &(Operator::GTE, Value::String("abc")), true), /* region column might
                                                                       * contain something ≥
                                                                       * "abc" */
            ("region", &(Operator::GTE, Value::String("east")), true), /* region column might
                                                                        * contain something ≥
                                                                        * "east" */
            ("region", &(Operator::GTE, Value::String("west")), true), /* region column might
                                                                        * contain something ≥
                                                                        * "west" */
            ("region", &(Operator::GTE, Value::String("zoo")), false), /* region column can't
                                                                        * contain something ≥
                                                                        * "zoo" */
            ("region", &(Operator::LT, Value::String("foo")), true), /* region column might
                                                                      * contain something <
                                                                      * "foo" */
            ("region", &(Operator::LT, Value::String("north")), true), /* region column might
                                                                        * contain something <
                                                                        * "north" */
            ("region", &(Operator::LT, Value::String("south")), true), /* region column might
                                                                        * contain something <
                                                                        * "south" */
            ("region", &(Operator::LT, Value::String("east")), false), /* region column can't
                                                                        * contain something <
                                                                        * "east" */
            ("region", &(Operator::LT, Value::String("abc")), false), /* region column can't
                                                                       * contain something <
                                                                       * "abc" */
            ("region", &(Operator::LTE, Value::String("east")), true), /* region column might
                                                                        * contain something ≤
                                                                        * "east" */
            ("region", &(Operator::LTE, Value::String("north")), true), /* region column might
                                                                         * contain something ≤
                                                                         * "north" */
            ("region", &(Operator::LTE, Value::String("south")), true), /* region column might
                                                                         * contain something ≤
                                                                         * "south" */
            ("region", &(Operator::LTE, Value::String("abc")), false), /* region column can't
                                                                        * contain something ≤
                                                                        * "abc" */
        ];

        for (column_name, predicate, exp) in cases {
            assert_eq!(
                row_group.column_could_satisfy_predicate(column_name, predicate),
                exp,
                "({:?}, {:?}) failed",
                column_name,
                predicate
            );
        }
    }

    #[test]
    fn pack_unpack_group_keys() {
        let cases = vec![
            vec![0, 0, 0, 0],
            vec![1, 2, 3, 4],
            vec![1, 3, 4, 2],
            vec![0],
            vec![0, 1],
            vec![u32::MAX, u32::MAX, u32::MAX, u32::MAX],
            vec![u32::MAX, u16::MAX as u32, u32::MAX, u16::MAX as u32],
            vec![0, u16::MAX as u32, 0],
            vec![0, u16::MAX as u32, 0, 0],
            vec![0, 0, u32::MAX, 0],
        ];

        for case in cases {
            let mut packed_value = 0_u128;
            for (i, &encoded_id) in case.iter().enumerate() {
                packed_value = pack_u32_in_u128(packed_value, encoded_id, i);
            }

            assert_eq!(
                unpack_u128_group_key(packed_value, case.len(), vec![]),
                case
            );
        }
    }

    #[test]
    fn read_group_result() {
        let group_columns = vec!["region", "host"];
        let aggregate_columns = vec![
            ("temp", AggregateType::Sum),
            ("voltage", AggregateType::Count),
        ];

        let result = ReadGroupResult {
            group_columns,
            aggregate_columns,
            group_keys: vec![
                GroupKey(vec![Value::String("east"), Value::String("host-a")]),
                GroupKey(vec![Value::String("east"), Value::String("host-b")]),
                GroupKey(vec![Value::String("west"), Value::String("host-a")]),
                GroupKey(vec![Value::String("west"), Value::String("host-c")]),
                GroupKey(vec![Value::String("west"), Value::String("host-d")]),
            ],
            aggregates: vec![
                vec![
                    AggregateResult::Sum(Scalar::I64(10)),
                    AggregateResult::Count(3),
                ],
                vec![
                    AggregateResult::Sum(Scalar::I64(20)),
                    AggregateResult::Count(4),
                ],
                vec![
                    AggregateResult::Sum(Scalar::I64(25)),
                    AggregateResult::Count(3),
                ],
                vec![
                    AggregateResult::Sum(Scalar::I64(21)),
                    AggregateResult::Count(1),
                ],
                vec![
                    AggregateResult::Sum(Scalar::I64(11)),
                    AggregateResult::Count(9),
                ],
            ],
        };

        // Debug implementation
        assert_eq!(
            format!("{:?}", &result),
            "region,host,temp_sum,voltage_count
east,host-a,10,3
east,host-b,20,4
west,host-a,25,3
west,host-c,21,1
west,host-d,11,9
"
        );

        // Display implementation
        assert_eq!(
            format!("{}", &result),
            "east,host-a,10,3
east,host-b,20,4
west,host-a,25,3
west,host-c,21,1
west,host-d,11,9
"
        );
    }
}
