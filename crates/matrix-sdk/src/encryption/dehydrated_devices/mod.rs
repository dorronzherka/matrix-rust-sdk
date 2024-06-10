// Copyright 2023 The Matrix.org Foundation C.I.C.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use matrix_sdk_base::crypto::dehydrated_devices::{DehydrationError, RehydratedDevice};
use ruma::{api::client::dehydrated_device::{self, get_events, DehydratedDeviceData}, serde::Raw, DeviceId, OwnedDeviceId};
use crate::{Client, Error};
/// The dehyrdated manager for the [`Client`].
#[derive(Debug, Clone)]
pub struct DehydratedDevices {
    pub (super) client:Client,
}


/// Submodule for Dehydrated devices
impl DehydratedDevices {
    
    /// Create new dehydrated Device
    pub async fn create(&self, pickle_key: [u8; 32]) -> dehydrated_device::put_dehydrated_device::unstable::Request   {
        let future  = async {
            let olm_machine = self.client.olm_machine().await;
            let olm_machine = olm_machine.as_ref().ok_or(Error::NoOlmMachine).unwrap();
            let dehydrated_devices = olm_machine.dehydrated_devices();
            let dehydrated_device = dehydrated_devices.create().await.unwrap();
            let req: dehydrated_device::put_dehydrated_device::unstable::Request = dehydrated_device.keys_for_upload("dehyrdrated_device".to_owned(), &pickle_key).await.unwrap();
            let _ = self.client.send(req.clone(), None).await;
            return req
    
        };
   
        future.await
    }


    /// Rehydrate the dehyrated device
    pub async fn rehydrate(&self, pickle_key: &[u8; 32], device_id: &DeviceId, device_data: Raw<DehydratedDeviceData>) -> Result<RehydratedDevice, DehydrationError> {
        let future = async {
            let olm_machine = self.client.olm_machine().await;
            let olm_machine = olm_machine.as_ref().ok_or(Error::NoOlmMachine).unwrap();
            let dehydrated_devices = olm_machine.dehydrated_devices();
            dehydrated_devices.rehydrate(pickle_key, device_id, device_data).await
        };

        future.await
    }
    
    /// Get events of rehydrated device
    pub async fn get_events_for_rehyrdated_device(&self, device_id: OwnedDeviceId) ->  Result<get_events::unstable::Response, crate::HttpError> {
        let future = async {
            let rq = get_events::unstable::Request::new(device_id);
            let res: Result<get_events::unstable::Response, crate::HttpError> =  self.client.send(rq, None).await;
            return res
         
        };

        future.await
    }
}