use crate::entities::FieldType;
use crate::services::cell::{CellCache, CellDataChangeset, CellDataDecoder, CellProtobufBlob};
use crate::services::field::{
  CellDataProtobufEncoder, TypeOption, TypeOptionCellData, TypeOptionCellDataCompare,
  TypeOptionCellDataFilter, TypeOptionTransform,
};
use crate::services::sort::SortCondition;
use collab::preclude::Any;
use collab_database::fields::Field;
use collab_database::fields::checkbox_type_option::CheckboxTypeOption;
use collab_database::fields::checklist_type_option::ChecklistTypeOption;
use collab_database::fields::date_type_option::{DateTypeOption, TimeTypeOption};
use collab_database::fields::media_type_option::MediaTypeOption;
use collab_database::fields::number_type_option::NumberTypeOption;
use collab_database::fields::relation_type_option::RelationTypeOption;
use collab_database::fields::select_type_option::{MultiSelectTypeOption, SingleSelectTypeOption};
use collab_database::fields::summary_type_option::SummarizationTypeOption;
use collab_database::fields::text_type_option::RichTextTypeOption;
use collab_database::fields::timestamp_type_option::TimestampTypeOption;
use collab_database::fields::translate_type_option::TranslateTypeOption;
use collab_database::fields::url_type_option::URLTypeOption;
use collab_database::rows::{Cell, RowId, get_field_type_from_cell};
use flowy_error::FlowyResult;
use lib_infra::box_any::BoxAny;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

pub const CELL_DATA: &str = "data";

/// Each [FieldType] has its own [TypeOptionCellDataHandler].
/// A helper trait that used to erase the `Self` of `TypeOption` trait to make it become a Object-safe trait
/// Only object-safe traits can be made into trait objects.
///
/// Object-safe traits are traits with methods that follow these two rules:
///
/// 1. the return type is not Self.
/// 2. there are no generic types parameters.
///
pub trait TypeOptionCellDataHandler: Send + Sync + 'static {
  /// Format the cell to [BoxCellData] using the passed-in [FieldType] and [Field].
  /// The caller can get the cell data by calling [BoxCellData::unbox_or_none].
  fn handle_get_boxed_cell_data(&self, cell: &Cell, field: &Field) -> Option<BoxCellData>;

  fn handle_get_protobuf_cell_data(
    &self,
    cell: &Cell,
    field_rev: &Field,
  ) -> FlowyResult<CellProtobufBlob>;

  fn handle_cell_changeset(
    &self,
    cell_changeset: BoxAny,
    old_cell: Option<Cell>,
    field: &Field,
  ) -> FlowyResult<Cell>;

  /// Compares two cell data values given their optional references, field information, and sorting condition.
  ///
  /// This function is designed to handle the comparison of cells that might not be initialized. The cells are
  /// first decoded based on the provided field type, and then compared according to the specified sort condition.
  ///
  /// # Parameters
  /// - `left_cell`: An optional reference to the left cell's data.
  /// - `right_cell`: An optional reference to the right cell's data.
  /// - `field`: A reference to the field information, which includes details about the field type.
  /// - `sort_condition`: The condition that dictates the sort order based on the results of the comparison.
  ///
  /// # Returns
  /// An `Ordering` indicating:
  /// - `Ordering::Equal` if both cells are `None` or if their decoded values are equal.
  /// - `Ordering::Less` or `Ordering::Greater` based on the `apply_cmp_with_uninitialized` or `apply_cmp`
  ///   method results and the specified `sort_condition`.
  ///
  /// # Note
  /// - If only one of the cells is `None`, the other cell is decoded, and the comparison is made using
  ///   the `apply_cmp_with_uninitialized` method.
  /// - If both cells are present, they are decoded, and the comparison is made using the `apply_cmp` method.
  fn handle_cell_compare(
    &self,
    left_cell: Option<&Cell>,
    right_cell: Option<&Cell>,
    field: &Field,
    sort_condition: SortCondition,
  ) -> Ordering;

  fn handle_cell_filter(&self, field: &Field, cell: &Cell, filter: &BoxAny) -> bool;

  /// Stringify the cell according to the field_type of this handler.
  ///
  /// For example, if the field type of the [TypeOptionCellDataHandler] is [FieldType::Date], then the string will be a formatted string according to
  /// the type option of the field. It might be something like "Mar 14, 2022".
  fn handle_stringify_cell(&self, cell: &Cell, field: &Field) -> String;

  fn handle_numeric_cell(&self, cell: &Cell) -> Option<f64>;

  fn handle_is_empty(&self, cell: &Cell, field: &Field) -> bool;
}

#[derive(Debug)]
struct CellDataCacheKey(u64);
impl CellDataCacheKey {
  pub fn new(field_rev: &Field, decoded_field_type: FieldType, cell: &Cell) -> Self {
    let mut hasher = DefaultHasher::new();
    if let Some(type_option_data) = field_rev.get_any_type_option(decoded_field_type) {
      map_hash(&type_option_data, &mut hasher);
    }
    hasher.write(field_rev.id.as_bytes());
    hasher.write_u8(decoded_field_type as u8);
    map_hash(cell, &mut hasher);
    Self(hasher.finish())
  }
}

fn any_hash<H: Hasher>(any: &Any, hasher: &mut H) {
  //FIXME: this is very bad idea for hash calculation
  match any {
    Any::Null | Any::Undefined => hasher.write_u8(0),
    Any::Bool(v) => v.hash(hasher),
    Any::Number(v) => v.to_be_bytes().hash(hasher),
    Any::BigInt(v) => v.hash(hasher),
    Any::String(v) => v.hash(hasher),
    Any::Buffer(v) => v.hash(hasher),
    Any::Array(v) => {
      for v in v.iter() {
        any_hash(v, hasher);
      }
    },
    Any::Map(v) => map_hash(v, hasher),
  }
}

fn map_hash<H: Hasher>(map: &HashMap<String, Any>, hasher: &mut H) {
  for (k, v) in map.iter() {
    k.hash(hasher);
    any_hash(v, hasher);
  }
}

impl AsRef<u64> for CellDataCacheKey {
  fn as_ref(&self) -> &u64 {
    &self.0
  }
}

struct TypeOptionCellDataHandlerImpl<T> {
  inner: T,
  field_type: FieldType,
  cell_data_cache: Option<CellCache>,
}

impl<T> TypeOptionCellDataHandlerImpl<T>
where
  T: TypeOption
    + CellDataDecoder
    + CellDataChangeset
    + CellDataProtobufEncoder
    + TypeOptionTransform
    + TypeOptionCellDataFilter
    + TypeOptionCellDataCompare
    + Send
    + Sync
    + 'static,
{
  pub fn into_boxed(self) -> Box<dyn TypeOptionCellDataHandler> {
    Box::new(self) as Box<dyn TypeOptionCellDataHandler>
  }

  pub fn new_with_boxed(
    inner: T,
    field_type: FieldType,
    cell_data_cache: Option<CellCache>,
  ) -> Box<dyn TypeOptionCellDataHandler> {
    Self {
      inner,
      field_type,
      cell_data_cache,
    }
    .into_boxed()
  }
}

impl<T> TypeOptionCellDataHandlerImpl<T>
where
  T: TypeOption + CellDataDecoder + Send + Sync,
{
  fn get_cell_data_cache_key(&self, cell: &Cell, field: &Field) -> CellDataCacheKey {
    CellDataCacheKey::new(field, self.field_type, cell)
  }

  fn get_cell_data_from_cache(&self, cell: &Cell, field: &Field) -> Option<T::CellData> {
    let key = self.get_cell_data_cache_key(cell, field);
    let cell_data_cache = self.cell_data_cache.as_ref()?;
    let cell = cell_data_cache.get::<T::CellData>(key.as_ref())?;
    Some(cell.value().clone())
  }

  fn set_cell_data_in_cache(&self, cell: &Cell, cell_data: T::CellData, field: &Field) {
    if let Some(cell_data_cache) = self.cell_data_cache.as_ref() {
      let field_type = FieldType::from(field.field_type);
      let key = CellDataCacheKey::new(field, field_type, cell);
      // tracing::trace!(
      //   "Cell cache update: field_type:{}, cell: {:?}, cell_data: {:?}",
      //   field_type,
      //   cell,
      //   cell_data
      // );
      cell_data_cache.insert(key.as_ref(), cell_data);
    }
  }

  fn get_cell_data(&self, cell: &Cell, field: &Field) -> Option<T::CellData> {
    let field_type_of_cell = get_field_type_from_cell(cell)?;
    if let Some(cell_data) = self.get_cell_data_from_cache(cell, field) {
      return Some(cell_data);
    }

    // If the field type of the cell is the same as the field type of the handler, we can directly decode the cell.
    // Otherwise, we need to transform the cell to the field type of the handler.
    let cell_data = if field_type_of_cell == self.field_type {
      Some(self.decode_cell(cell).unwrap_or_default())
    } else if is_type_option_cell_transformable(field_type_of_cell, self.field_type) {
      Some(
        self
          .decode_cell_with_transform(cell, field_type_of_cell, field)
          .unwrap_or_default(),
      )
    } else {
      None
    };

    if let Some(data) = &cell_data {
      self.set_cell_data_in_cache(cell, data.clone(), field);
    }

    cell_data
  }
}

impl<T> std::ops::Deref for TypeOptionCellDataHandlerImpl<T> {
  type Target = T;

  fn deref(&self) -> &Self::Target {
    &self.inner
  }
}

impl<T> TypeOptionCellDataHandler for TypeOptionCellDataHandlerImpl<T>
where
  T: TypeOption
    + CellDataDecoder
    + CellDataChangeset
    + CellDataProtobufEncoder
    + TypeOptionTransform
    + TypeOptionCellDataFilter
    + TypeOptionCellDataCompare
    + Send
    + Sync
    + 'static,
{
  fn handle_get_boxed_cell_data(&self, cell: &Cell, field: &Field) -> Option<BoxCellData> {
    let cell_data = self.get_cell_data(cell, field)?;
    Some(BoxCellData::new(cell_data))
  }

  fn handle_get_protobuf_cell_data(
    &self,
    cell: &Cell,
    field_rev: &Field,
  ) -> FlowyResult<CellProtobufBlob> {
    let cell_data = self.get_cell_data(cell, field_rev).unwrap_or_default();
    CellProtobufBlob::from(self.protobuf_encode(cell_data))
  }

  fn handle_cell_changeset(
    &self,
    cell_changeset: BoxAny,
    old_cell: Option<Cell>,
    field: &Field,
  ) -> FlowyResult<Cell> {
    let changeset = cell_changeset.unbox_or_error::<T::CellChangeset>()?;
    let (cell, cell_data) = self.apply_changeset(changeset, old_cell)?;
    self.set_cell_data_in_cache(&cell, cell_data, field);
    Ok(cell)
  }

  fn handle_cell_compare(
    &self,
    left_cell: Option<&Cell>,
    right_cell: Option<&Cell>,
    field: &Field,
    sort_condition: SortCondition,
  ) -> Ordering {
    match (left_cell, right_cell) {
      (None, None) => Ordering::Equal,
      (None, Some(right_cell)) => {
        let right_cell_data = self.get_cell_data(right_cell, field).unwrap_or_default();

        self.apply_cmp_with_uninitialized(None, Some(right_cell_data).as_ref(), sort_condition)
      },
      (Some(left_cell), None) => {
        let left_cell_data = self.get_cell_data(left_cell, field).unwrap_or_default();

        self.apply_cmp_with_uninitialized(Some(left_cell_data).as_ref(), None, sort_condition)
      },
      (Some(left_cell), Some(right_cell)) => {
        let left_cell_data = self.get_cell_data(left_cell, field).unwrap_or_default();
        let right_cell_data = self.get_cell_data(right_cell, field).unwrap_or_default();

        self.apply_cmp(&left_cell_data, &right_cell_data, sort_condition)
      },
    }
  }

  fn handle_cell_filter(&self, field: &Field, cell: &Cell, filter: &BoxAny) -> bool {
    let perform_filter = || {
      let cell_filter = filter.downcast_ref::<T::CellFilter>()?;
      let cell_data = self.get_cell_data(cell, field).unwrap_or_default();
      Some(self.apply_filter(cell_filter, &cell_data))
    };

    perform_filter().unwrap_or(true)
  }

  /// Stringify [Cell] to string
  /// if the [TypeOptionCellDataHandler] supports transform, it will try to transform the [Cell] to
  /// the passed-in field type [Cell].
  /// For example, the field type of the [TypeOptionCellDataHandler] is [FieldType::MultiSelect], the field_type
  /// is [FieldType::RichText], then the string will be transformed to a string that separated by comma with the
  /// option's name.
  ///
  fn handle_stringify_cell(&self, cell: &Cell, field: &Field) -> String {
    if is_type_option_cell_transformable(self.field_type, FieldType::RichText) {
      let cell_data = self.get_cell_data(cell, field);
      if let Some(cell_data) = cell_data {
        return self.stringify_cell_data(cell_data);
      }
    }
    "".to_string()
  }

  fn handle_numeric_cell(&self, cell: &Cell) -> Option<f64> {
    self.numeric_cell(cell)
  }

  fn handle_is_empty(&self, cell: &Cell, field: &Field) -> bool {
    let cell_data = self.get_cell_data(cell, field).unwrap_or_default();

    cell_data.is_cell_empty()
  }
}

pub struct TypeOptionCellExt<'a> {
  field: &'a Field,
  cell_data_cache: Option<CellCache>,
}

impl<'a> TypeOptionCellExt<'a> {
  pub fn new(field: &'a Field, cell_data_cache: Option<CellCache>) -> Self {
    Self {
      field,
      cell_data_cache,
    }
  }

  pub fn get_type_option_cell_data_handler_with_field_type(
    &self,
    field_type: FieldType,
  ) -> Option<Box<dyn TypeOptionCellDataHandler>> {
    match field_type {
      FieldType::RichText => self
        .field
        .get_type_option::<RichTextTypeOption>(field_type)
        .map(|type_option| {
          TypeOptionCellDataHandlerImpl::new_with_boxed(
            type_option,
            field_type,
            self.cell_data_cache.clone(),
          )
        }),
      FieldType::Number => self
        .field
        .get_type_option::<NumberTypeOption>(field_type)
        .map(|type_option| {
          TypeOptionCellDataHandlerImpl::new_with_boxed(
            type_option,
            field_type,
            self.cell_data_cache.clone(),
          )
        }),
      FieldType::DateTime => self
        .field
        .get_type_option::<DateTypeOption>(field_type)
        .map(|type_option| {
          TypeOptionCellDataHandlerImpl::new_with_boxed(
            type_option,
            field_type,
            self.cell_data_cache.clone(),
          )
        }),
      FieldType::LastEditedTime | FieldType::CreatedTime => self
        .field
        .get_type_option::<TimestampTypeOption>(field_type)
        .map(|type_option| {
          TypeOptionCellDataHandlerImpl::new_with_boxed(
            type_option,
            field_type,
            self.cell_data_cache.clone(),
          )
        }),
      FieldType::SingleSelect => self
        .field
        .get_type_option::<SingleSelectTypeOption>(field_type)
        .map(|type_option| {
          TypeOptionCellDataHandlerImpl::new_with_boxed(
            type_option,
            field_type,
            self.cell_data_cache.clone(),
          )
        }),
      FieldType::MultiSelect => self
        .field
        .get_type_option::<MultiSelectTypeOption>(field_type)
        .map(|type_option| {
          TypeOptionCellDataHandlerImpl::new_with_boxed(
            type_option,
            field_type,
            self.cell_data_cache.clone(),
          )
        }),
      FieldType::Checkbox => self
        .field
        .get_type_option::<CheckboxTypeOption>(field_type)
        .map(|type_option| {
          TypeOptionCellDataHandlerImpl::new_with_boxed(
            type_option,
            field_type,
            self.cell_data_cache.clone(),
          )
        }),
      FieldType::URL => {
        self
          .field
          .get_type_option::<URLTypeOption>(field_type)
          .map(|type_option| {
            TypeOptionCellDataHandlerImpl::new_with_boxed(
              type_option,
              field_type,
              self.cell_data_cache.clone(),
            )
          })
      },
      FieldType::Checklist => self
        .field
        .get_type_option::<ChecklistTypeOption>(field_type)
        .map(|type_option| {
          TypeOptionCellDataHandlerImpl::new_with_boxed(
            type_option,
            field_type,
            self.cell_data_cache.clone(),
          )
        }),
      FieldType::Relation => self
        .field
        .get_type_option::<RelationTypeOption>(field_type)
        .map(|type_option| {
          TypeOptionCellDataHandlerImpl::new_with_boxed(
            type_option,
            field_type,
            self.cell_data_cache.clone(),
          )
        }),
      FieldType::Summary => self
        .field
        .get_type_option::<SummarizationTypeOption>(field_type)
        .map(|type_option| {
          TypeOptionCellDataHandlerImpl::new_with_boxed(
            type_option,
            field_type,
            self.cell_data_cache.clone(),
          )
        }),
      FieldType::Time => self
        .field
        .get_type_option::<TimeTypeOption>(field_type)
        .map(|type_option| {
          TypeOptionCellDataHandlerImpl::new_with_boxed(
            type_option,
            field_type,
            self.cell_data_cache.clone(),
          )
        }),
      FieldType::Translate => self
        .field
        .get_type_option::<TranslateTypeOption>(field_type)
        .map(|type_option| {
          TypeOptionCellDataHandlerImpl::new_with_boxed(
            type_option,
            field_type,
            self.cell_data_cache.clone(),
          )
        }),
      FieldType::Media => self
        .field
        .get_type_option::<MediaTypeOption>(field_type)
        .map(|type_option| {
          TypeOptionCellDataHandlerImpl::new_with_boxed(
            type_option,
            field_type,
            self.cell_data_cache.clone(),
          )
        }),
    }
  }

  pub fn get_type_option_cell_data_handler(&self) -> Option<Box<dyn TypeOptionCellDataHandler>> {
    let field_type = FieldType::from(self.field.field_type);
    self.get_type_option_cell_data_handler_with_field_type(field_type)
  }
}

/// when return true, the to_field_type must implement [CellDataDecoder]'s decode_cell_with_transform
pub fn is_type_option_cell_transformable(
  from_field_type: FieldType,
  to_field_type: FieldType,
) -> bool {
  matches!(
    (from_field_type, to_field_type),
    // Checkbox
    (FieldType::Checkbox, FieldType::SingleSelect)
      | (FieldType::Checkbox, FieldType::MultiSelect)
    // SingleSelect or MultiSelect
      | (FieldType::SingleSelect, FieldType::MultiSelect)
      | (FieldType::MultiSelect, FieldType::SingleSelect)
    // Text
      | (FieldType::RichText, FieldType::SingleSelect)
      | (FieldType::RichText, FieldType::MultiSelect)
      | (FieldType::RichText, FieldType::URL)
      | (FieldType::RichText, FieldType::Number)
      | (FieldType::RichText, FieldType::DateTime)
      | (_, FieldType::RichText)
  )
}

pub type BoxCellData = BoxAny;

pub struct RowSingleCellData {
  pub row_id: RowId,
  pub field_id: String,
  pub field_type: FieldType,
  pub cell_data: Option<BoxCellData>,
}

macro_rules! into_cell_data {
  ($func_name:ident,$return_ty:ty) => {
    #[allow(dead_code)]
    pub fn $func_name(self) -> Option<$return_ty> {
      self.cell_data?.unbox_or_none()
    }
  };
}

impl RowSingleCellData {
  into_cell_data!(
    into_text_field_cell_data,
    <RichTextTypeOption as TypeOption>::CellData
  );
  into_cell_data!(
    into_number_field_cell_data,
    <NumberTypeOption as TypeOption>::CellData
  );
  into_cell_data!(
    into_url_field_cell_data,
    <URLTypeOption as TypeOption>::CellData
  );
  into_cell_data!(
    into_single_select_field_cell_data,
    <SingleSelectTypeOption as TypeOption>::CellData
  );
  into_cell_data!(
    into_multi_select_field_cell_data,
    <MultiSelectTypeOption as TypeOption>::CellData
  );
  into_cell_data!(
    into_date_field_cell_data,
    <DateTypeOption as TypeOption>::CellData
  );
  into_cell_data!(
    into_timestamp_field_cell_data,
    <TimestampTypeOption as TypeOption>::CellData
  );
  into_cell_data!(
    into_check_list_field_cell_data,
    <CheckboxTypeOption as TypeOption>::CellData
  );
}
