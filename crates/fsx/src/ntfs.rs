use std::fs::File;

pub fn data_logical_size(file: &ntfs::NtfsFile, device: &mut File) -> u64 {
    let Some(Ok(data_item)) = file.data(device, "") else {
        return 0;
    };
    let Ok(data_attr) = data_item.to_attribute() else {
        return 0;
    };
    data_attr
        .value(device)
        .map(|value| value.len())
        .unwrap_or(0)
}

pub fn data_allocated_size(file: &ntfs::NtfsFile, device: &mut File, block_size: u64) -> u64 {
    let Some(Ok(data_item)) = file.data(device, "") else {
        return 0;
    };
    let Ok(data_attr) = data_item.to_attribute() else {
        return 0;
    };
    if data_attr.is_resident() {
        return round_up(data_attr.value_length(), block_size);
    }
    let Ok(value) = data_attr.value(device) else {
        return 0;
    };
    match value {
        ntfs::attribute_value::NtfsAttributeValue::NonResident(value) => value
            .data_runs()
            .flatten()
            .filter(|run| run.data_position().value().is_some())
            .map(|run| run.allocated_size())
            .sum(),
        ntfs::attribute_value::NtfsAttributeValue::AttributeListNonResident(value) => {
            round_up(value.len(), block_size)
        }
        ntfs::attribute_value::NtfsAttributeValue::Resident(value) => {
            round_up(value.len(), block_size)
        }
    }
}

fn round_up(size: u64, block_size: u64) -> u64 {
    if size == 0 || block_size == 0 {
        return size;
    }
    size.saturating_add(block_size - 1) / block_size * block_size
}

pub fn best_filename(
    entry: &ntfs::NtfsIndexEntry<'_, ntfs::indexes::NtfsFileNameIndex>,
) -> Option<String> {
    let file_name = entry.key()?.ok()?;
    match file_name.namespace() {
        ntfs::structured_values::NtfsFileNamespace::Dos => None,
        _ => Some(file_name.name().to_string_lossy().to_string()),
    }
}

pub fn is_reparse_point(file: &ntfs::NtfsFile, device: &mut File) -> bool {
    let mut attributes = file.attributes();
    while let Some(attribute_result) = attributes.next(device) {
        if let Ok(attribute_item) = attribute_result
            && let Ok(attribute) = attribute_item.to_attribute()
            && let Ok(attribute_type) = attribute.ty()
            && attribute_type == ntfs::NtfsAttributeType::ReparsePoint
        {
            return true;
        }
    }
    false
}
