use faer::linalg::solvers::SolverCore;
use faer::prelude::*;
use faer::Side;
use faer_ext::{IntoFaer, IntoNdarray};
use ndarray::{array, s, Array, Array1, Array2, ArrayView1, Axis, NewAxis};
use std::cmp::max;
use std::str::FromStr;

#[cfg(any(target_os = "macos", target_os = "linux"))]
use ndarray_linalg::LeastSquaresSvd;

/// Invert square matrix input using either Cholesky or LU decomposition
pub fn inv(array: &Array2<f64>, use_cholesky: bool) -> Array2<f64> {
    let m = array.view().into_faer();
    if use_cholesky {
        match m.cholesky(Side::Lower) {
            Ok(cholesky) => {
                return cholesky.inverse().as_ref().into_ndarray().to_owned();
            }
            Err(_) => {
                println!("Cholesky decomposition failed, falling back to LU decomposition");
            }
        }
    }
    // fall back to LU decomposition
    m.partial_piv_lu()
        .inverse()
        .as_ref()
        .into_ndarray()
        .to_owned()
}

#[derive(PartialEq)]
pub enum SolveMethod {
    QR,
    SVD,
    Cholesky,
    LU,
    CD, // coordinate-descent for elastic net problem
}

impl FromStr for SolveMethod {
    type Err = ();

    fn from_str(input: &str) -> Result<SolveMethod, Self::Err> {
        match input {
            "qr" => Ok(SolveMethod::QR),
            "svd" => Ok(SolveMethod::SVD),
            "chol" => Ok(SolveMethod::Cholesky),
            "lu" => Ok(SolveMethod::LU),
            "cd" => Ok(SolveMethod::CD),
            _ => Err(()),
        }
    }
}

/// Solves ridge regression using Singular Value Decomposition (SVD).
///
/// # Arguments
///
/// * `y` - Target vector.
/// * `x` - Feature matrix.
/// * `alpha` - Ridge parameter.
/// * `rcond` - Relative condition number used to determine cutoff for small singular values.
///
/// # Returns
///
/// * Result of ridge regression as a 1-dimensional array.
fn solve_ridge_svd(
    y: &Array1<f64>,
    x: &Array2<f64>,
    alpha: f64,
    rcond: Option<f64>,
) -> Array1<f64> {
    let x_faer = x.view().into_faer();
    let y_faer = y.view().insert_axis(Axis(1)).into_faer();

    // compute SVD and extract u, s, vt
    let svd = x_faer.thin_svd();
    let u = svd.u();
    let v = svd.v().into_ndarray();
    let s = svd.s_diagonal();

    // convert s into ndarray
    let s: Array1<f64> = s.as_2d().into_ndarray().slice(s![.., 0]).into_owned();
    let max_value = s.iter().skip(1).copied().fold(s[0], f64::max);

    // set singular values less than or equal to ``rcond * largest_singular_value`` to zero.
    let cutoff =
        rcond.unwrap_or(f64::EPSILON * max(x_faer.ncols(), x_faer.nrows()) as f64) * max_value;
    let s = s.map(|v| if v < &cutoff { 0. } else { *v });

    let binding = u.transpose() * y_faer;
    let u_t_y: Array1<f64> = binding
        .as_ref()
        .into_ndarray()
        .slice(s![.., 0])
        .into_owned();
    let d = &s / (&s * &s + alpha);
    let d_ut_y = &d * &u_t_y;
    v.dot(&d_ut_y)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn solve_ols_svd(y: &Array1<f64>, x: &Array2<f64>, rcond: Option<f64>) -> Array1<f64> {
    // TODO: try to compute w/ LAPACK SVD. Must handle BLAS dependency on linux & windows OS
    //      either use ndarray-linalg or directly call sgelsd from lapack crate..
    solve_ridge_svd(y, x, 1.0e-64, rcond) // near zero ridge penalty
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[allow(unused_variables)]
fn solve_ols_svd(y: &Array1<f64>, x: &Array2<f64>, rcond: Option<f64>) -> Array1<f64> {
    x.least_squares(y)
        .expect("Failed to compute LAPACK SVD solution!")
        .solution
}

/// Solves an ordinary least squares problem using either QR (faer) or LAPACK SVD
/// Inputs: features (2d ndarray), targets (1d ndarray), and an optional enum denoting solve method
/// Outputs: 1-d OLS coefficients
pub fn solve_ols(
    y: &Array1<f64>,
    x: &Array2<f64>,
    solve_method: Option<SolveMethod>,
    rcond: Option<f64>,
) -> Array1<f64> {
    let n_features = x.len_of(Axis(1));
    let n_samples = x.len_of(Axis(0));

    let solve_method = match solve_method {
        Some(SolveMethod::QR) => SolveMethod::QR,
        Some(SolveMethod::SVD) => SolveMethod::SVD,
        None => {
            // automatically determine recommended solution method based on shape of data
            if n_samples > n_features {
                SolveMethod::QR
            } else {
                SolveMethod::SVD
            }
        }
        _ => panic!("Only 'QR' and 'SVD' are currently supported solve methods for OLS."),
    };

    if solve_method == SolveMethod::QR {
        // compute least squares solution via QR
        let x_faer = x.view().into_faer();
        let y_faer = y.slice(s![.., NewAxis]).into_faer();
        let coefficients = x_faer.col_piv_qr().solve_lstsq(&y_faer);
        coefficients
            .as_ref()
            .into_ndarray()
            .slice(s![.., 0])
            .to_owned()
    } else {
        solve_ols_svd(y, x, rcond)
    }
}

/// Solves the normal equations: (X^T X) coefficients = X^T Y
/// Attempts to solve with either Cholesky or LU (partial pivoting)
fn solve_normal_equations(xtx: &Array2<f64>, xty: &Array1<f64>, use_cholesky: bool) -> Array1<f64> {
    // Attempt to solve via Cholesky decomposition
    let xtx_faer = xtx.view().into_faer();
    if use_cholesky {
        match xtx_faer.cholesky(Side::Lower) {
            Ok(cholesky) => {
                // Cholesky decomposition successful
                return cholesky
                    .solve(&xty.slice(s![.., NewAxis]).into_faer())
                    .as_ref()
                    .into_ndarray()
                    .slice(s![.., 0])
                    .into_owned();
            }
            Err(_) => {
                // Cholesky decomposition failed, fallback to LU decomposition w/ partial pivoting
                println!("Cholesky decomposition failed, falling back to LU decomposition");
            }
        }
    }
    // Fall back to LU decomposition w/ partial pivoting
    xtx_faer
        .partial_piv_lu()
        .solve(&xty.slice(s![.., NewAxis]).into_faer())
        .as_ref()
        .into_ndarray()
        .slice(s![.., 0])
        .into_owned()
}

/// Solves a ridge regression problem of the form: ||y - x B|| + alpha * ||B||
/// Inputs: features (2d ndarray), targets (1d ndarray), ridge alpha scalar
pub fn solve_ridge(
    y: &Array1<f64>,
    x: &Array2<f64>,
    alpha: f64,
    solve_method: Option<SolveMethod>,
    rcond: Option<f64>,
) -> Array1<f64> {
    assert!(alpha >= 0., "alpha must be non-negative");
    match solve_method {
        Some(SolveMethod::Cholesky) | Some(SolveMethod::LU) | None => {
            let x_t = &x.t();
            let x_t_x = x_t.dot(x);
            let x_t_y = x_t.dot(y);
            let eye = Array::eye(x_t_x.shape()[0]);
            let ridge_matrix = &x_t_x + &eye * alpha;
            // use cholesky if specifically chosen, and otherwise LU.
            solve_normal_equations(
                &ridge_matrix,
                &x_t_y,
                solve_method == Some(SolveMethod::Cholesky),
            )
        }
        Some(SolveMethod::SVD) => solve_ridge_svd(y, x, alpha, rcond),
        _ => panic!(
            "Only 'Cholesky', 'LU', & 'SVD' are currently supported solver \
        methods for Ridge."
        ),
    }
}

fn soft_threshold(x: &f64, alpha: f64, positive: bool) -> f64 {
    let mut result = x.signum() * (x.abs() - alpha).max(0.0);
    if positive {
        result = result.max(0.0);
    }
    result
}

/// Solves an elastic net regression problem of the form: 1 / (2 * n_samples) * ||y - Xw||_2
/// + alpha * l1_ratio * ||w||_1 + 0.5 * alpha * (1 - l1_ratio) * ||w||_2.
/// Uses cyclic coordinate descent with efficient 'naive updates' and a
/// general soft thresholding function.
#[allow(clippy::too_many_arguments)]
pub fn solve_elastic_net(
    y: &Array1<f64>,
    x: &Array2<f64>,
    alpha: f64,            // strictly positive regularization parameter
    l1_ratio: Option<f64>, // scalar strictly between 0 (full ridge) and 1 (full lasso)
    max_iter: Option<usize>,
    tol: Option<f64>,       // controls convergence criteria between iterations
    positive: Option<bool>, // enforces non-negativity constraint
    solve_method: Option<SolveMethod>,
) -> Array1<f64> {
    let l1_ratio = l1_ratio.unwrap_or(0.5);
    let max_iter = max_iter.unwrap_or(1_000);
    let tol = tol.unwrap_or(0.00001);
    let positive = positive.unwrap_or(false);

    match solve_method {
        Some(SolveMethod::CD) => {}
        None => {}
        _ => panic!(
            "Only solve_method 'CD' (coordinate descent) is currently supported \
        for Elastic Net / Lasso problems."
        ),
    }
    assert!(alpha > 0., "'alpha' must be strictly positive");
    assert!(
        (0. ..=1.).contains(&l1_ratio),
        "'l1_ratio' must be strictly between 0. and 1."
    );

    let (n_samples, n_features) = (x.shape()[0], x.shape()[1]);
    let mut w = Array1::<f64>::zeros(n_features);
    let xtx = x.t().dot(x);
    let mut residuals = y.to_owned(); // Initialize residuals
    let alpha = alpha * n_samples as f64;

    for _ in 0..max_iter {
        let w_old = w.clone();
        for j in 0..n_features {
            let xj = x.slice(s![.., j]);
            // Naive update: add contribution of current feature to residuals
            residuals = &residuals + &xj * w[j];
            w[j] = soft_threshold(&xj.dot(&residuals.view()), alpha * l1_ratio, positive)
                / (xtx[[j, j]] + alpha * (1.0 - l1_ratio));
            // Naive update: subtract contribution of current feature from residuals
            residuals = &residuals - &xj * w[j];
        }
        if (&w - &w_old)
            .view()
            .insert_axis(Axis(0))
            .into_faer()
            .norm_l2()
            < tol
        {
            break;
        }
    }
    w
}

pub struct RecursiveLeastSquares {
    forgetting_factor: f64, // exponential decay factor
    coef: Array1<f64>,      // coefficient vector
    p: Array2<f64>,         // state covariance
    k: Array1<f64>,         // kalman gain
}

impl RecursiveLeastSquares {
    pub fn new(
        num_features: usize,
        lam: f64,
        half_life: Option<f64>,
        initial_state_mean: Option<Array1<f64>>,
    ) -> Self {
        // calculate forgetting_factor based on the value of half_life, default to 1.0
        // (expanding ols)
        let forgetting_factor = if let Some(half_life) = half_life {
            (0.5f64.ln() / half_life).exp()
        } else {
            1.0
        };

        let coef = Array1::<f64>::zeros(num_features);
        let p = Array2::<f64>::eye(num_features) * lam;
        let k = Array1::<f64>::zeros(num_features);
        let coef = initial_state_mean.unwrap_or(coef);
        RecursiveLeastSquares {
            forgetting_factor,
            coef,
            p,
            k,
        }
    }

    pub fn update(&mut self, x: &Array1<f64>, y: f64) {
        let r = 1.0 + x.t().dot(&self.p).dot(x) / self.forgetting_factor;
        self.k
            .assign(&(&self.p.dot(x) / (r * self.forgetting_factor)));
        let residuals = y - x.dot(&self.coef);
        self.coef.assign(&(&self.coef + &(&self.k * residuals)));
        let k_ = &self.k.view().insert_axis(Axis(1)); // K x 1
        self.p
            .assign(&(&self.p / self.forgetting_factor - k_.dot(&k_.t()) * r));
    }

    pub fn predict(&self, x: &Array1<f64>) -> f64 {
        x.dot(&self.coef)
    }
}

/// Solves an online least squares problem updating coefficients with every sample.
///
/// This function performs online least squares regression, updating the coefficients
/// with every sample. It uses the Recursive Least Squares (RLS) algorithm to adaptively
/// update the coefficients as new data arrives.
///
/// # Arguments
/// * `y` - A reference to a one-dimensional array containing the target values.
/// * `x` - A reference to a two-dimensional array containing the input features.
/// * `half_life` - An optional parameter representing the half-life of forgetting past information
///                 in the Recursive Least Squares algorithm. A smaller half-life places more
///                 weight on recent samples.
/// * `initial_state_covariance` - An optional parameter representing the initial covariance
///                                 matrix of the state estimation. Default value is 10.0.
/// * `initial_state_mean` - An optional parameter representing the initial mean vector of the
///                           state estimation. If not provided, it is initialized to zeros.
///
/// # Returns
/// A two-dimensional array containing the updated coefficients of the linear regression model.
pub fn solve_recursive_least_squares(
    y: &Array1<f64>,
    x: &Array2<f64>,
    half_life: Option<f64>,
    initial_state_covariance: Option<f64>,
    initial_state_mean: Option<Array1<f64>>,
    is_valid: &[bool],
) -> Array2<f64> {
    let (n_samples, n_features) = (x.shape()[0], x.shape()[1]);
    let mut recursive_least_squares = RecursiveLeastSquares::new(
        n_features,
        initial_state_covariance.unwrap_or(10.0),
        half_life,
        initial_state_mean,
    );
    // let mut predictions = Array1::<f64>::zeros(n_samples);
    let mut coefficients = Array2::<f64>::zeros((n_samples, n_features));

    for t in 0..n_samples {
        let y_t = y[t];
        let x_t = x.slice(s![t, ..]).to_owned();
        if is_valid[t] {
            recursive_least_squares.update(&x_t, y_t);
        }
        coefficients
            .slice_mut(s![t, ..])
            .assign(&recursive_least_squares.coef.view());
        // predictions[t] = recursive_least_squares.predict(&x_t);
    }
    coefficients
}

pub fn outer_product(u: &ArrayView1<f64>, v: &ArrayView1<f64>) -> Array2<f64> {
    // Reshape u and v to have a shape of (n, 1) and (1, m) respectively
    let u_reshaped = u.insert_axis(Axis(1));
    let v_reshaped = v.insert_axis(Axis(0));

    // Compute the outer product using broadcasting and dot product
    u_reshaped.dot(&v_reshaped)
}

fn inv_diag(c: &Array2<f64>) -> Array2<f64> {
    let s = c.raw_dim();
    assert!(s[0] == s[1]);
    let mut res: Array2<f64> = Array2::zeros(s);
    for i in 0..s[0] {
        res[(i, i)] = c[(i, i)].recip();
    }
    res
}

/// Computes the Woodbury update of a matrix A_inv using matrices U, V, B_inv.
///
/// The Woodbury update is a method to efficiently update the inverse of a matrix A_inv
/// when adding or removing rows or columns from the original matrix.
///
/// The Woodbury update formula is given by:
///
/// ```text
/// (A + U C V)^{-1} = A^{-1} - A^{-1} U (C^{-1} + V A^{-1} U)^{-1} V A^{-1}
/// ```
pub fn woodbury_update(
    a_inv: &Array2<f64>,
    u: &Array2<f64>,
    c: &Array2<f64>,
    v: &Array2<f64>,
    c_is_diag: Option<bool>,
) -> Array2<f64> {
    // Check if c_is_diag is Some(true)
    let inv_c = if let Some(true) = c_is_diag {
        inv_diag(c)
    } else {
        inv(c, false)
    }; // r x r
       // compute V inv(A)
    let v_inv_a = v.dot(a_inv); // r x K
    let inv_a_u = a_inv.dot(u); // K x r
                                // compute term (C^{-1} + V A^{-1} U)^{-1}
    let intermediate = inv(&(inv_c + v.dot(&inv_a_u)), false); // r x r
    a_inv - inv_a_u.dot(&intermediate).dot(&v_inv_a) // K x K
}

/// Function to update inv(X^TX) by x_update array of rank r using Woodbury Identity.
pub fn update_xtx_inv(
    xtx_inv: &Array2<f64>,
    x_update: &Array2<f64>,
    c: Option<&Array2<f64>>,
) -> Array2<f64> {
    // Reshape x_new and x_old for Woodbury update
    let u = x_update.t().to_owned(); // K x r
    let v = u.t().to_owned(); // r x K

    // let c = Array2::eye(u.shape()[1]); // Identity matrix r x r
    let default = Array2::eye(u.shape()[1]);
    let c = c.unwrap_or(&default);

    // Apply Woodbury update
    woodbury_update(xtx_inv, &u, c, &v, Some(true))
}

/// Solves rolling ordinary least squares (OLS) regression.
///
/// This function calculates the coefficients of the linear regression model
/// using rolling windows. It takes a dependent variable `y`, an independent variable matrix `x`,
/// the size of the rolling window, the minimum number of periods required to calculate
/// coefficients, and an optional flag to specify whether to use Woodbury matrix identity
/// for additional efficiency in case of large number of features.
///
/// # Arguments
///
/// * `y` - A reference to a 1-dimensional array representing the dependent variable.
/// * `x` - A reference to a 2-dimensional array representing the independent variables.
/// * `window_size` - The size of the rolling window.
/// * `min_periods` - An optional parameter specifying the minimum number of periods
///                   required to calculate coefficients. If not provided, it defaults to 1.
/// * `use_woodbury` - An optional parameter specifying whether to use Woodbury matrix identity
///                    which propagates inv(XTX) directly. If not provided, it defaults to `false`.
///
pub fn solve_rolling_ols(
    y: &Array1<f64>,
    x: &Array2<f64>,
    window_size: usize,
    min_periods: Option<usize>,
    use_woodbury: Option<bool>,
    alpha: Option<f64>,
) -> Array2<f64> {
    let n = x.shape()[0];
    let k = x.shape()[1]; // Number of independent variables
    let min_periods = min_periods.unwrap_or(std::cmp::min(k, window_size));
    // default to using woodbury if number of features is relatively large.
    let use_woodbury = use_woodbury.unwrap_or(k > 60);
    let mut coefficients = Array2::from_elem((n, k), f64::NAN);
    let alpha = alpha.unwrap_or(0.0);

    // we allow the user to pass a min_periods < k, but this may result in
    // unstable warm-up coefficients - so warn the user.
    if !(min_periods >= k && min_periods <= window_size) {
        println!(
            "warning: min_periods should be greater or equal to the number of regressors \
                  in the model and less than or equal to the window size otherwise \
                  estimated parameters may be unstable!"
        )
    };

    // Initialize X^T X, inv(X.T X), and X^T Y
    let x_warmup = x.slice(s![..min_periods, ..]);
    let y_warmup = y.slice(s![..min_periods]);
    let mut xty = x_warmup.t().dot(&y_warmup);
    let mut xtx = x_warmup.t().dot(&x_warmup);

    // add ridge penalty
    if alpha > 0. {
        xtx = xtx + Array2::<f64>::eye(k) * alpha
    }

    // Use woodbury to propagate inv(X.T X) & (X.T Y)
    if use_woodbury {
        // assign warm-up coefficients
        let mut xtx_inv = inv(&xtx, false);
        let coef_warmup = xtx_inv.t().dot(&xty);
        coefficients
            .slice_mut(s![min_periods - 1, ..])
            .assign(&coef_warmup);

        // make c [[-1, 0], [0, 1]]; which drops old and adds new
        let c: Array2<f64> = array![[-1., 0.], [0., 1.]];

        // Slide the window and update coefficients
        for i in min_periods..n {
            let i_start = i.saturating_sub(window_size);
            let x_new = x.row(i);

            if i > window_size - 1 {
                let x_prev = x.row(i_start);

                // create rank 2 update array
                let mut x_update = ndarray::stack(Axis(0), &[x_prev, x_new]).unwrap(); // 2 x K

                // multiply x_old row by -1.0 (subtract the previous contribution)
                x_update.row_mut(0).mapv_inplace(|elem| -elem);

                // update inv(XTX) and XTY
                xtx_inv = update_xtx_inv(&xtx_inv, &x_update, Some(&c));
                xty = xty + &x_new * y[i]  // add new contribution
                    - &x_prev * y[i_start] // subtract old contribution
                ;
            } else {
                let x_update = x_new.insert_axis(Axis(0)).into_owned(); // 1 x K
                xtx_inv = update_xtx_inv(&xtx_inv, &x_update, None);
                xty = xty + &x_new * y[i];
            }
            coefficients.slice_mut(s![i, ..]).assign(&xtx_inv.dot(&xty));
        }
    } else {
        // update X.T X & X.T Y and solve normal equations at every time step
        // assign warm-up coefficients
        let coef_warmup = solve_normal_equations(&xtx, &xty, false);

        coefficients
            .slice_mut(s![min_periods - 1, ..])
            .assign(&coef_warmup);

        // Slide the window and update coefficients
        for i in min_periods..n {
            let i_start = i.saturating_sub(window_size);
            // update XTX w/ latest data point
            let x_new = x.row(i);

            // Add new contributions
            xtx += &outer_product(&x_new, &x_new);
            xty = xty + &x_new * y[i];

            // Subtract the previous contribution
            if i > window_size - 1 {
                let x_prev = x.row(i_start);
                xtx -= &outer_product(&x_prev, &x_prev);
                xty = xty - &x_prev * y[i_start];
            }

            // update coefficients
            let coefficients_i = solve_normal_equations(&xtx, &xty, true);
            coefficients.slice_mut(s![i, ..]).assign(&coefficients_i);
        }
    }
    coefficients
}
