use crate::errors::WorldError;
use crate::errors::WorldError::InvalidBlockStateData;
use crate::vanilla_chunk_format;
use crate::vanilla_chunk_format::VanillaChunk;
use bitcode_derive::{Decode, Encode};
use deepsize::DeepSizeOf;
use ferrumc_general_purpose::data_packing::i32::read_nbit_i32;
use ferrumc_macros::{NBTDeserialize, NBTSerialize};
use ferrumc_net_codec::net_types::var_int::VarInt;
use lazy_static::lazy_static;
use std::cmp::max;
use std::collections::HashMap;
use std::io::Read;
use tracing::{debug, error, warn};
use vanilla_chunk_format::BlockData;

#[cfg(test)]
const BLOCKSFILE: &[u8] = &[0];

// If this file doesn't exist, you'll have to create it yourself. Download the 1.21.1 server from the
// minecraft launcher, extract the blocks data (info here https://minecraft.wiki/w/Minecraft_Wiki:Projects/wiki.vg_merge/Data_Generators#Blocks_report)
// , put the blocks.json file in the .etc folder, and run the blocks_parser.py script in the scripts
// folder. This will generate the blockmappings.json file that is compressed with bzip2 and included
// in the binary.
#[cfg(not(test))]
const BLOCKSFILE: &[u8] = include_bytes!("../../../../.etc/blockmappings.bz2");

lazy_static! {
    pub static ref ID2BLOCK: HashMap<i32, BlockData> = {
        let mut bzipreader = bzip2::read::BzDecoder::new(BLOCKSFILE);
        let mut output = String::new();
        bzipreader.read_to_string(&mut output).unwrap();
        let string_keys: HashMap<String, BlockData> = serde_json::from_str(&output).unwrap();
        string_keys
            .iter()
            .map(|(k, v)| (k.parse::<i32>().unwrap(), v.clone()))
            .collect()
    };
    pub static ref BLOCK2ID: HashMap<BlockData, i32> =
        ID2BLOCK.iter().map(|(k, v)| (v.clone(), *k)).collect();
}

#[derive(Encode, Decode, Clone, DeepSizeOf)]
// This is a placeholder for the actual chunk format
pub struct Chunk {
    pub x: i32,
    pub z: i32,
    pub dimension: String,
    pub sections: Vec<Section>,
    pub heightmaps: Heightmaps,
}

#[derive(Encode, Decode, NBTDeserialize, NBTSerialize, Clone, DeepSizeOf)]
#[nbt(net_encode)]
pub struct Heightmaps {
    #[nbt(rename = "MOTION_BLOCKING")]
    pub motion_blocking: Vec<i64>,
    #[nbt(rename = "WORLD_SURFACE")]
    pub world_surface: Vec<i64>,
}
#[derive(Encode, Decode, Clone, DeepSizeOf)]
pub struct Section {
    pub y: i8,
    pub block_states: BlockStates,
    pub biome_states: BiomeStates,
    pub block_light: Vec<u8>,
    pub sky_light: Vec<u8>,
}
#[derive(Encode, Decode, Clone, DeepSizeOf)]
pub struct BlockStates {
    pub non_air_blocks: u16,
    pub block_data: PaletteType,
    pub block_counts: HashMap<BlockData, i32>,
}

#[derive(Encode, Decode, Clone, DeepSizeOf)]
pub enum PaletteType {
    Single(VarInt),
    Indirect {
        bits_per_block: u8,
        data: Vec<i64>,
        palette: Vec<VarInt>,
    },
    Direct {
        bits_per_block: u8,
        data: Vec<i64>,
    },
}

#[derive(Encode, Decode, Clone, DeepSizeOf)]
pub struct BiomeStates {
    pub bits_per_biome: u8,
    pub data: Vec<i64>,
    pub palette: Vec<VarInt>,
}

fn convert_to_net_palette(vanilla_palettes: Vec<BlockData>) -> Result<Vec<VarInt>, WorldError> {
    let mut new_palette = Vec::new();
    for palette in vanilla_palettes {
        if let Some(id) = BLOCK2ID.get(&palette) {
            new_palette.push(VarInt::from(*id));
        } else {
            new_palette.push(VarInt::from(0));
            error!("Could not find block id for palette entry: {:?}", palette);
        }
    }
    Ok(new_palette)
}

impl Heightmaps {
    pub fn new() -> Self {
        Heightmaps {
            motion_blocking: vec![],
            world_surface: vec![],
        }
    }
}

impl Default for Heightmaps {
    fn default() -> Self {
        Heightmaps::new()
    }
}

impl VanillaChunk {
    pub fn to_custom_format(&self) -> Result<Chunk, WorldError> {
        let mut sections = Vec::new();
        for section in self.sections.as_ref().unwrap() {
            let y = section.y;
            let mut block_data: PaletteType;
            let raw_block_data = section
                .block_states
                .as_ref()
                .and_then(|bs| bs.data.clone())
                .unwrap_or_default();
            let palette = section
                .block_states
                .as_ref()
                .and_then(|bs| bs.palette.clone())
                .unwrap_or_default();
            let bits_per_block = max((palette.len() as f32).log2().ceil() as u8, 4);
            let mut block_counts = HashMap::new();
            for chunk in &raw_block_data {
                let mut i = 0;
                while i + bits_per_block < 64 {
                    let palette_index = read_nbit_i32(chunk, bits_per_block as usize, i as u32)?;
                    let block = match palette.get(palette_index as usize) {
                        Some(block) => block,
                        None => {
                            error!("Could not find block for palette index: {}", palette_index);
                            &BlockData::default()
                        }
                    };
                    *block_counts.entry(block.clone()).or_insert(0) += 1;
                    i += bits_per_block;
                }
            }
            if raw_block_data.is_empty() {
                let single_block = BlockData::default();
                block_data = PaletteType::Single(VarInt::from(0));
                block_counts.insert(single_block.clone(), 4096);
            } else {
                block_data = PaletteType::Indirect {
                    bits_per_block,
                    data: raw_block_data,
                    palette: convert_to_net_palette(palette)?,
                };
            }
            // Count the number of blocks that are either air, void air, or cave air
            let mut air_blocks = *block_counts.get(&BlockData::default()).unwrap_or(&0) as u16;
            air_blocks += *block_counts
                .get(&BlockData {
                    name: "minecraft:void_air".to_string(),
                    properties: None,
                })
                .unwrap_or(&0) as u16;
            air_blocks += *block_counts
                .get(&BlockData {
                    name: "minecraft:cave_air".to_string(),
                    properties: None,
                })
                .unwrap_or(&0) as u16;
            let non_air_blocks = 4096 - air_blocks;
            let block_states = BlockStates {
                block_counts,
                non_air_blocks,
                block_data,
            };
            let block_light = section
                .block_light
                .clone()
                .unwrap_or(vec![0; 2048])
                .iter()
                .map(|x| *x as u8)
                .collect();
            let sky_light = section
                .sky_light
                .clone()
                .unwrap_or(vec![0; 2048])
                .iter()
                .map(|x| *x as u8)
                .collect();
            let biome_states = BiomeStates {
                // TODO: Implement biome states properly
                bits_per_biome: 4,
                data: vec![],
                palette: vec![VarInt::from(0); 1],
            };
            let section = Section {
                y,
                block_states,
                biome_states,
                block_light,
                sky_light,
            };
            sections.push(section);
        }
        let heightmaps = if let Some(heightmaps) = &self.heightmaps {
            let motion_blocking = heightmaps.clone().motion_blocking.unwrap_or(vec![]);
            let world_surface = heightmaps.clone().world_surface.unwrap_or(vec![]);
            Heightmaps {
                motion_blocking,
                world_surface,
            }
        } else {
            Heightmaps {
                motion_blocking: vec![],
                world_surface: vec![],
            }
        };
        Ok(Chunk {
            x: self.x_pos,
            z: self.z_pos,
            dimension: self.clone().dimension.unwrap_or("overworld".to_string()),
            sections,
            heightmaps,
        })
    }
}

impl BlockStates {
    pub fn resize(&mut self, new_bit_size: usize) -> Result<(), WorldError> {
        match &mut self.block_data {
            PaletteType::Single(val) => {
                let block = ID2BLOCK
                    .get(&val.val)
                    .cloned()
                    .unwrap_or(BlockData::default());
                let mut new_palette = vec![VarInt::from(0); 1];
                if let Some(id) = BLOCK2ID.get(&block) {
                    new_palette[0] = VarInt::from(*id);
                } else {
                    error!("Could not find block id for palette entry: {:?}", block);
                }
                self.block_data = PaletteType::Indirect {
                    bits_per_block: new_bit_size as u8,
                    data: vec![],
                    palette: new_palette,
                }
            }
            PaletteType::Indirect {
                bits_per_block,
                data,
                palette,
            } => {
                // Step 1: Read existing packed data into a list of normal integers
                let mut normalised_ints = Vec::with_capacity(4096);
                let mut values_read = 0;

                for long in data {
                    let mut bit_offset = 0;

                    while bit_offset + *bits_per_block as usize <= 64 {
                        if values_read >= 4096 {
                            break;
                        }

                        // Extract value at the current bit offset
                        let value =
                            read_nbit_i32(&long, *bits_per_block as usize, bit_offset as u32)?;
                        let max_int_value = (1 << new_bit_size) - 1;
                        if value > max_int_value {
                            return Err(InvalidBlockStateData(format!(
                                "Value {} exceeds maximum value for {}-bit block state",
                                value, new_bit_size
                            )));
                        }
                        normalised_ints.push(value);
                        values_read += 1;

                        bit_offset += *bits_per_block as usize;
                    }

                    // Stop reading if we’ve already hit 4096 values
                    if values_read >= 4096 {
                        break;
                    }
                }

                // Check if we read exactly 4096 block states
                if normalised_ints.len() != 4096 {
                    return Err(InvalidBlockStateData(format!(
                        "Expected 4096 block states, but got {}",
                        normalised_ints.len()
                    )));
                }

                // Step 2: Write the normalised integers into the new packed format
                let mut new_data = Vec::new();
                let mut current_long: i64 = 0;
                let mut bit_position = 0;

                for &value in &normalised_ints {
                    current_long |= (value as i64) << bit_position;
                    bit_position += new_bit_size;

                    if bit_position >= 64 {
                        new_data.push(current_long);
                        current_long = (value as i64) >> (new_bit_size - (bit_position - 64));
                        bit_position -= 64;
                    }
                }

                // Push any remaining bits in the final long
                if bit_position > 0 {
                    new_data.push(current_long);
                }

                // Verify the size of the new data matches expectations
                let expected_size = (4096 * new_bit_size).div_ceil(64);
                if new_data.len() != expected_size {
                    return Err(InvalidBlockStateData(format!(
                        "Expected packed data size of {}, but got {}",
                        expected_size,
                        new_data.len()
                    )));
                }
                // Update the chunk with the new packed data and bit size
                self.block_data = PaletteType::Indirect {
                    bits_per_block: new_bit_size as u8,
                    data: new_data,
                    palette: palette.clone(),
                }
            }
            _ => {
                todo!("Implement resizing for direct palette")
            }
        };
        Ok(())
    }
}

impl Chunk {
    /// Sets the block at the specified coordinates to the specified block data.
    /// If the block is the same as the old block, nothing happens.
    /// If the block is not in the palette, it is added.
    /// If the palette is in single block mode, it is converted to palette'd mode.
    ///
    /// # Arguments
    ///
    /// * `x` - The x-coordinate of the block.
    /// * `y` - The y-coordinate of the block.
    /// * `z` - The z-coordinate of the block.
    /// * `block` - The block data to set the block to.
    ///
    /// # Returns
    ///
    /// * `Ok(())` - If the block was successfully set.
    /// * `Err(WorldError)` - If an error occurs while setting the block.
    ///
    /// ### Note
    /// The positions are modulo'd by 16 to get the block index in the section anyway, so converting
    /// the coordinates to section coordinates isn't really necessary, but you should probably do it
    /// anyway for readability's sake.
    pub fn set_block(
        &mut self,
        x: i32,
        y: i32,
        z: i32,
        block: BlockData,
    ) -> Result<(), WorldError> {
        // Get old block
        let old_block = self.get_block(x, y, z)?;
        if old_block == block {
            debug!("Block is the same as the old block");
            return Ok(());
        }
        // Get section
        let section = self
            .sections
            .iter_mut()
            .find(|section| section.y == (y >> 4) as i8)
            .ok_or(WorldError::SectionOutOfBounds(y >> 4))?;
        match &mut section.block_states.block_data {
            PaletteType::Single(val) => {
                debug!("Converting single block to indirect palette");
                // If it's a single block, convert it to indirect then re-run the function
                section.block_states.block_data = PaletteType::Indirect {
                    bits_per_block: 4,
                    data: vec![0; 256],
                    palette: vec![val.clone()],
                };
                self.set_block(x, y, z, block)?;
            }
            PaletteType::Indirect {
                bits_per_block,
                data,
                palette,
            } => {
                let block_counts = &mut section.block_states.block_counts;
                match block_counts.get_mut(&old_block) {
                    Some(e) => {
                        if *e <= 0 {
                            return Err(WorldError::InvalidBlock(old_block));
                        }
                        *e -= 1;
                    }
                    None => {
                        warn!("Block not found in block counts: {:?}", old_block);
                    }
                }
                let block_id = BLOCK2ID
                    .get(&block)
                    .ok_or(WorldError::InvalidBlock(block.clone()))?;
                // Add new block
                if let Some(e) = section.block_states.block_counts.get(&block) {
                    section.block_states.block_counts.insert(block, e + 1);
                } else {
                    debug!("Adding block to block counts");
                    section.block_states.block_counts.insert(block, 1);
                }
                // Get block index
                let block_palette_index = palette
                    .iter()
                    .position(|p| p.val == *block_id)
                    .unwrap_or_else(|| {
                        // Add block to palette if it doesn't exist
                        let index = palette.len() as i16;
                        palette.push((*block_id).into());
                        index as usize
                    });
                // Set block
                let blocks_per_i64 = (64f64 / *bits_per_block as f64).floor() as usize;
                let index = ((y & 0xf) * 256 + (z & 0xf) * 16 + (x & 0xf)) as usize;
                let i64_index = index / blocks_per_i64;
                let packed_u64 = data
                    .get_mut(i64_index)
                    .ok_or(InvalidBlockStateData(format!(
                        "Invalid block state data at index {}",
                        i64_index
                    )))?;
                let offset = (index % blocks_per_i64) * *bits_per_block as usize;
                if let Err(e) = ferrumc_general_purpose::data_packing::u32::write_nbit_u32(
                    packed_u64,
                    offset as u32,
                    block_palette_index as u32,
                    *bits_per_block,
                ) {
                    return Err(InvalidBlockStateData(format!(
                        "Failed to write block: {}",
                        e
                    )));
                }
            }
            PaletteType::Direct { .. } => {
                todo!("Implement direct palette for set_block");
            }
        }

        Ok(())
    }

    /// Gets the block at the specified coordinates.
    ///
    /// # Arguments
    ///
    /// * `x` - The x-coordinate of the block.
    /// * `y` - The y-coordinate of the block.
    /// * `z` - The z-coordinate of the block.
    ///
    /// # Returns
    ///
    /// * `Ok(BlockData)` - The block data at the specified coordinates.
    /// * `Err(WorldError)` - If an error occurs while retrieving the block data.
    ///
    /// ### Note
    /// The positions are modulo'd by 16 to get the block index in the section anyway, so converting
    /// the coordinates to section coordinates isn't really necessary, but you should probably do it
    /// anyway for readability's sake.
    pub fn get_block(&self, x: i32, y: i32, z: i32) -> Result<BlockData, WorldError> {
        let section = self
            .sections
            .iter()
            .find(|section| section.y == (y >> 4) as i8)
            .ok_or(WorldError::SectionOutOfBounds(y >> 4))?;
        match &section.block_states.block_data {
            PaletteType::Single(val) => {
                let block_id = val.val;
                ID2BLOCK
                    .get(&block_id)
                    .cloned()
                    .ok_or(WorldError::ChunkNotFound)
            }
            PaletteType::Indirect {
                bits_per_block,
                data,
                palette,
            } => {
                if palette.len() == 1 || *bits_per_block == 0 {
                    return ID2BLOCK
                        .get(&palette[0].val)
                        .cloned()
                        .ok_or(WorldError::ChunkNotFound);
                }
                let blocks_per_i64 = (64f64 / *bits_per_block as f64).floor() as usize;
                let index = ((y & 0xf) * 256 + (z & 0xf) * 16 + (x & 0xf)) as usize;
                let i64_index = index / blocks_per_i64;
                let packed_u64 = data.get(i64_index).ok_or(InvalidBlockStateData(format!(
                    "Invalid block state data at index {}",
                    i64_index
                )))?;
                let offset = (index % blocks_per_i64) * *bits_per_block as usize;
                let id = ferrumc_general_purpose::data_packing::u32::read_nbit_u32(
                    packed_u64,
                    *bits_per_block,
                    offset as u32,
                )?;
                let palette_id = palette.get(id as usize).ok_or(WorldError::ChunkNotFound)?;
                Ok(crate::chunk_format::ID2BLOCK
                    .get(&palette_id.val)
                    .unwrap_or(&BlockData::default())
                    .clone())
            }
            &PaletteType::Direct { .. } => todo!("Implement direct palette for get_block"),
        }
    }

    pub fn new(x: i32, z: i32, dimension: String) -> Self {
        let mut sections: Vec<Section> = (0..24)
            .map(|y| Section {
                y: y as i8,
                block_states: BlockStates {
                    non_air_blocks: 0,
                    block_data: PaletteType::Single(VarInt::from(0)),
                    block_counts: HashMap::from([(BlockData::default(), 4096)]),
                },
                biome_states: BiomeStates {
                    bits_per_biome: 0,
                    data: vec![],
                    palette: vec![VarInt::from(0)],
                },
                block_light: vec![255; 2048],
                sky_light: vec![255; 2048],
            })
            .collect();
        // for section in &mut sections {
        //     section.optimise().expect("Failed to optimise section");
        // }
        Chunk {
            x,
            z,
            dimension,
            sections,
            heightmaps: Heightmaps::new(),
        }
    }

    /// Sets the section at the specified index to the specified block data.
    /// If the section is out of bounds, an error is returned.
    ///
    /// # Arguments
    ///
    /// * `section` - The index of the section to set.
    /// * `block` - The block data to set the section to.
    ///
    /// # Returns
    ///
    /// * `Ok(())` - If the section was successfully set.
    /// * `Err(WorldError)` - If an error occurs while setting the section.
    pub fn set_section(&mut self, section: u8, block: BlockData) -> Result<(), WorldError> {
        if let Some(section) = self.sections.get_mut(section as usize) {
            section.fill(block)
        } else {
            Err(WorldError::SectionOutOfBounds(section as i32))
        }
    }

    /// Fills the chunk with the specified block.
    ///
    /// # Arguments
    ///
    /// * `block` - The block data to fill the chunk with.
    ///
    /// # Returns
    ///
    /// * `Ok(())` - If the chunk was successfully filled.
    /// * `Err(WorldError)` - If an error occurs while filling the chunk.
    pub fn fill(&mut self, block: BlockData) -> Result<(), WorldError> {
        for section in &mut self.sections {
            section.fill(block.clone())?;
        }
        Ok(())
    }
}

impl Section {
    /// Fills the section with the specified block.
    ///
    /// # Arguments
    ///
    /// * `block` - The block data to fill the section with.
    ///
    /// # Returns
    ///
    /// * `Ok(())` - If the section was successfully filled.
    /// * `Err(WorldError)` - If an error occurs while filling the section.
    pub fn fill(&mut self, block: BlockData) -> Result<(), WorldError> {
        let block_id = BLOCK2ID
            .get(&block)
            .ok_or(WorldError::InvalidBlock(block.clone()))?;
        self.block_states.block_data = PaletteType::Single(VarInt::from(*block_id));
        self.block_states.block_counts = HashMap::from([(block.clone(), 4096)]);
        if ["minecraft:air", "minecraft:void_air", "minecraft:cave_air"]
            .contains(&block.name.as_str())
        {
            self.block_states.non_air_blocks = 0;
        } else {
            self.block_states.non_air_blocks = 4096;
        }
        Ok(())
    }

    /// This function trims out unnecessary data from the section. Primarily it does 2 things:
    ///
    /// 1. Removes any palette entries that are not used in the block states data.
    ///
    /// 2. If there is only one block in the palette, it converts the palette to single block mode.
    pub fn optimise(&mut self) -> Result<(), WorldError> {
        match &mut self.block_states.block_data {
            PaletteType::Single(_) => {
                // If the section is already in single block mode, there's nothing to optimise
                return Ok(());
            }
            PaletteType::Indirect {
                bits_per_block,
                data,
                palette,
            } => {
                // Remove empty blocks from palette
                let mut remove_indexes = Vec::new();
                for (block, count) in &self.block_states.block_counts {
                    if *count <= 0 {
                        let block_id = BLOCK2ID
                            .get(block)
                            .ok_or(WorldError::InvalidBlock(block.clone()))?;
                        let index = palette.iter().position(|p| p.val == *block_id);
                        if let Some(index) = index {
                            remove_indexes.push(index);
                        } else {
                            return Err(WorldError::InvalidBlock(block.clone()));
                        }
                    }
                }
                for index in remove_indexes {
                    // Decrement any data entries that are higher than the removed index
                    for data_point in &mut *data {
                        let mut i = 0;
                        while (i + *bits_per_block as usize) < 64 {
                            let block_index =
                                ferrumc_general_purpose::data_packing::u32::read_nbit_u32(
                                    data_point,
                                    *bits_per_block,
                                    i as u32,
                                )?;
                            if block_index > index as u32 {
                                ferrumc_general_purpose::data_packing::u32::write_nbit_u32(
                                    data_point,
                                    i as u32,
                                    block_index - 1,
                                    *bits_per_block,
                                )?;
                            }
                            i += *bits_per_block as usize;
                        }
                    }
                }

                {
                    // If there is only one block in the palette, convert to single block mode
                    if palette.len() == 1 {
                        let block = ID2BLOCK
                            .get(&palette[0].val)
                            .cloned()
                            .unwrap_or(BlockData::default());
                        self.block_states.block_data = PaletteType::Single(palette[0].clone());
                        self.block_states.block_counts.clear();
                        self.block_states.block_counts.insert(block, 4096);
                    }
                }
            }
            PaletteType::Direct { .. } => {
                todo!("Implement optimisation for direct palette");
            }
        };

        Ok(())
    }
}
