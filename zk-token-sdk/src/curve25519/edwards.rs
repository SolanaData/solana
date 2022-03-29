use bytemuck::{Pod, Zeroable};
pub use target_arch::*;

#[derive(Clone, Copy, Debug, Default, PartialEq, Pod, Zeroable)]
#[repr(transparent)]
pub struct PodEdwardsPoint(pub [u8; 32]);

#[cfg(not(target_arch = "bpf"))]
mod target_arch {
    use {
        super::*,
        crate::curve25519::{
            curve_syscall_traits::{GroupOperations, MultiScalarMultiplication, PointValidation},
            errors::Curve25519Error,
            scalar::PodScalar,
        },
        curve25519_dalek::{
            edwards::{CompressedEdwardsY, EdwardsPoint},
            scalar::Scalar,
            traits::VartimeMultiscalarMul,
        },
    };

    pub fn validate_edwards(point: &PodEdwardsPoint) -> Option<bool> {
        Some(point.validate_point())
    }

    pub fn add_edwards(
        left_point: &PodEdwardsPoint,
        right_point: &PodEdwardsPoint,
    ) -> Option<PodEdwardsPoint> {
        PodEdwardsPoint::add(left_point, right_point)
    }

    pub fn subtract_edwards(
        left_point: &PodEdwardsPoint,
        right_point: &PodEdwardsPoint,
    ) -> Option<PodEdwardsPoint> {
        PodEdwardsPoint::subtract(left_point, right_point)
    }

    pub fn multiply_edwards(
        scalar: &PodScalar,
        point: &PodEdwardsPoint,
    ) -> Option<PodEdwardsPoint> {
        PodEdwardsPoint::multiply(scalar, point)
    }

    pub fn multiscalar_multiply_edwards(
        scalars: Vec<&PodScalar>,
        points: Vec<&PodEdwardsPoint>,
    ) -> Option<PodEdwardsPoint> {
        PodEdwardsPoint::multiscalar_multiply(scalars, points)
    }

    impl From<&EdwardsPoint> for PodEdwardsPoint {
        fn from(point: &EdwardsPoint) -> Self {
            Self(point.compress().to_bytes())
        }
    }

    impl TryFrom<&PodEdwardsPoint> for EdwardsPoint {
        type Error = Curve25519Error;

        fn try_from(pod: &PodEdwardsPoint) -> Result<Self, Self::Error> {
            CompressedEdwardsY::from_slice(&pod.0)
                .decompress()
                .ok_or(Curve25519Error::PodConversion)
        }
    }

    impl PointValidation for PodEdwardsPoint {
        type Point = Self;

        fn validate_point(&self) -> bool {
            CompressedEdwardsY::from_slice(&self.0)
                .decompress()
                .is_some()
        }
    }

    impl GroupOperations for PodEdwardsPoint {
        type Scalar = PodScalar;
        type Point = Self;

        fn add(left_point: &Self, right_point: &Self) -> Option<Self> {
            let left_point: EdwardsPoint = left_point.try_into().ok()?;
            let right_point: EdwardsPoint = right_point.try_into().ok()?;

            let result = &left_point + &right_point;
            Some((&result).into())
        }

        fn subtract(left_point: &Self, right_point: &Self) -> Option<Self> {
            let left_point: EdwardsPoint = left_point.try_into().ok()?;
            let right_point: EdwardsPoint = right_point.try_into().ok()?;

            let result = &left_point - &right_point;
            Some((&result).into())
        }

        #[cfg(not(target_arch = "bpf"))]
        fn multiply(scalar: &PodScalar, point: &Self) -> Option<Self> {
            let scalar: Scalar = scalar.into();
            let point: EdwardsPoint = point.try_into().ok()?;

            let result = &scalar * &point;
            Some((&result).into())
        }
    }

    impl MultiScalarMultiplication for PodEdwardsPoint {
        type Scalar = PodScalar;
        type Point = Self;

        fn multiscalar_multiply(scalars: Vec<&PodScalar>, points: Vec<&Self>) -> Option<Self> {
            EdwardsPoint::optional_multiscalar_mul(
                scalars.into_iter().map(Scalar::from),
                points
                    .into_iter()
                    .map(|point| EdwardsPoint::try_from(point).ok()),
            )
            .map(|result| PodEdwardsPoint::from(&result))
        }
    }
}

#[cfg(target_arch = "bpf")]
mod target_arch {
    use {
        super::*,
        crate::curve25519::curve_syscall_traits::{sol_curve_validate_point, CURVE25519_EDWARDS},
    };

    pub fn validate_edwards(point: &PodEdwardsPoint) -> Option<bool> {
        let mut validate_result = 0u8;
        let result = unsafe {
            sol_curve_validate_point(
                CURVE25519_EDWARDS,
                &point.0 as *const u8,
                &mut validate_result,
            )
        };

        if result == 0 {
            Some(validate_result == 0)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::curve25519::scalar::PodScalar,
        curve25519_dalek::{
            constants::ED25519_BASEPOINT_POINT as G, edwards::EdwardsPoint, scalar::Scalar,
            traits::Identity,
        },
        rand::rngs::OsRng,
    };

    #[test]
    fn test_validate_edwards() {
        let pod = PodEdwardsPoint(G.compress().to_bytes());
        assert!(validate_edwards(&pod).unwrap());

        let invalid_bytes = [
            120, 140, 152, 233, 41, 227, 203, 27, 87, 115, 25, 251, 219, 5, 84, 148, 117, 38, 84,
            60, 87, 144, 161, 146, 42, 34, 91, 155, 158, 189, 121, 79,
        ];

        assert!(!validate_edwards(&PodEdwardsPoint(invalid_bytes)).unwrap());
    }

    #[test]
    fn test_edwards_add_subtract() {
        // identity
        let identity = PodEdwardsPoint(EdwardsPoint::identity().compress().to_bytes());
        let random_point = PodEdwardsPoint((Scalar::random(&mut OsRng) * G).compress().to_bytes());

        assert_eq!(add_edwards(&random_point, &identity).unwrap(), random_point);
        assert_eq!(
            subtract_edwards(&random_point, &identity).unwrap(),
            random_point
        );

        // associativity
        let point_a = PodEdwardsPoint((Scalar::random(&mut OsRng) * G).compress().to_bytes());
        let point_b = PodEdwardsPoint((Scalar::random(&mut OsRng) * G).compress().to_bytes());
        let point_c = PodEdwardsPoint((Scalar::random(&mut OsRng) * G).compress().to_bytes());

        assert_eq!(
            add_edwards(&add_edwards(&point_a, &point_b).unwrap(), &point_c),
            add_edwards(&point_a, &add_edwards(&point_b, &point_c).unwrap()),
        );

        assert_eq!(
            subtract_edwards(&subtract_edwards(&point_a, &point_b).unwrap(), &point_c),
            subtract_edwards(&point_a, &add_edwards(&point_b, &point_c).unwrap()),
        );

        // commutativity
        let point_a = PodEdwardsPoint((Scalar::random(&mut OsRng) * G).compress().to_bytes());
        let point_b = PodEdwardsPoint((Scalar::random(&mut OsRng) * G).compress().to_bytes());

        assert_eq!(
            add_edwards(&point_a, &point_b).unwrap(),
            add_edwards(&point_b, &point_a).unwrap(),
        );

        // subtraction
        let identity = PodEdwardsPoint(EdwardsPoint::identity().compress().to_bytes());
        let random_edwards = Scalar::random(&mut OsRng) * G;
        let random_point = PodEdwardsPoint(random_edwards.compress().to_bytes());
        let random_point_negated = PodEdwardsPoint((-random_edwards).compress().to_bytes());

        assert_eq!(
            random_point_negated,
            subtract_edwards(&identity, &random_point).unwrap(),
        )
    }

    #[test]
    fn test_edwards_mul() {
        let scalar_a = PodScalar(Scalar::random(&mut OsRng).to_bytes());
        let point_x = PodEdwardsPoint((Scalar::random(&mut OsRng) * G).compress().to_bytes());
        let point_y = PodEdwardsPoint((Scalar::random(&mut OsRng) * G).compress().to_bytes());

        let ax = multiply_edwards(&scalar_a, &point_x).unwrap();
        let bx = multiply_edwards(&scalar_a, &point_y).unwrap();

        assert_eq!(
            add_edwards(&ax, &bx),
            multiply_edwards(&scalar_a, &add_edwards(&point_x, &point_y).unwrap()),
        );
    }

    #[test]
    fn test_multiscalar_multiplication_edwards() {
        let scalar = PodScalar(Scalar::random(&mut OsRng).to_bytes());
        let point = PodEdwardsPoint((Scalar::random(&mut OsRng) * G).compress().to_bytes());

        let basic_product = multiply_edwards(&scalar, &point).unwrap();
        let msm_product = multiscalar_multiply_edwards(vec![&scalar], vec![&point]).unwrap();

        assert_eq!(basic_product, msm_product);

        let scalar_a = PodScalar(Scalar::random(&mut OsRng).to_bytes());
        let scalar_b = PodScalar(Scalar::random(&mut OsRng).to_bytes());
        let point_x = PodEdwardsPoint((Scalar::random(&mut OsRng) * G).compress().to_bytes());
        let point_y = PodEdwardsPoint((Scalar::random(&mut OsRng) * G).compress().to_bytes());

        let ax = multiply_edwards(&scalar_a, &point_x).unwrap();
        let by = multiply_edwards(&scalar_b, &point_y).unwrap();
        let basic_product = add_edwards(&ax, &by).unwrap();
        let msm_product =
            multiscalar_multiply_edwards(vec![&scalar_a, &scalar_b], vec![&point_x, &point_y])
                .unwrap();

        assert_eq!(basic_product, msm_product);
    }
}
